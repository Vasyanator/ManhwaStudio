"""
File: modules/ai_backend/inpaint/flux2_klein/service.py

Purpose:
`Flux2KleinInpaintService` - the object `server.py` puts on `AppState` and every
`inpaint.flux2_klein.*` IPC handler calls. It owns the service lock, the
in-memory prompt-embedding cache (with its provenance), the tokenizer /
scheduler caches and the request methods; WHERE the weights live during and between runs is decided by
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
  the pipeline's admission, `_combined_ceilings`, `runtime.large_models.paired` module notes), then the denoise and the VAE decode run as manager phases (a
  memory failure re-plans along the degrade ladder: tiled decode, lowered tile,
  a lower «Видеопамять» step);
- `component_action` - one per-component residency action, each a manager
  session (load / unload / park in RAM / back to the GPU / warm-up);
- `_prompt_plan` - THE answer to "which embedding serves each text this run
  needs, and which must be encoded", delegated to the shared orchestration
  (`runtime.prompt_cache_service.PromptCacheOrchestrator.plan` over FLUX.2's
  `prompt_cache.Flux2PromptCacheAdapter`): memory and the model's library
  ranked by `select_entry`; the run, `.status` (`prompt_cached`,
  `prompt_cache_entry`) and the encoder forecast all derive from it;
- the `prompt_cache_*` methods (`build` / `list` / `export` / `import`) - the
  library surface (protocol 23: named presets, one library per model) over
  the same orchestrator; this service adds the encoder session, the progress
  steps, the answers' extra keys and its own refusals;
- `large_model_profiles` - the adapter `large_models.profiles` calls: the
  pipeline's step pickers planned with the encoder's admission claimed (the
  two-handle composition is `runtime.large_models.paired.plan_pair` over
  `memory.FLUX2_PAIR`; the encoder counted unless it is neither needed nor
  resident: `_encoder_need` decides the request's `paired.EncoderNeed`), plus
  the encoder's own format rows (`encoder_formats`); both handles are planned
  in the request's selected formats (`large_model.weight_format_of`)
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
  module (`hardware._clear_torch_cache`, `prompt_library.serialize_tensor`,
  `attention.build_text_attention_mask`, ...).
- The memory cache (`runtime.prompt_cache_service.EmbeddingMemory`, count-capped
  by `PROMPT_EMBED_CACHE_ENTRIES`) is mutated only under the service lock; its
  provenance index is a copy-on-write `dict` that the lock-free `_prompt_plan`
  (status, forecast) reads.
- torch / diffusers / transformers are imported lazily.
"""

from __future__ import annotations

import logging
import os
import threading
import time
from collections.abc import Callable, Mapping
from pathlib import Path
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    import numpy as np

from ...runtime import prompt_library as pl
from ...runtime.health_snapshot import HealthSnapshot
from ...runtime.large_models import paired
from ...runtime.large_models.manager import LargeModelManager, MemoryLimitsStore, ProfilesReport, Session, SessionObserver
from ...runtime.large_models.paired import EncoderNeed, HandleRequest
from ...runtime.large_models.spec import ExecDevice
from ...runtime.large_models.spec import RequestShape
from ...runtime.large_models.tiers import RamStep, Steps, VramStep
from ...runtime.model_manager import SmallModelManager
from ...runtime.prompt_cache_service import (
    EmbeddingMemory,
    MemoryEmbedding,
    PromptCacheOrchestrator,
    PromptPlan,
    entry_info as _entry_info,
    require_prompt_to_cache,
)
from ...runtime.prompt_cache_service import (  # noqa: F401  - test seams reached through this module (`_test_fixtures.seed_memory`)
    memory_key as _memory_key,
    tensor_width as _tensor_width,
)
from ...runtime.residency.footprint import normalize_device
from ...runtime.residency.holdings import Pool
from ..region_imaging import decode_image_rgb, decode_mask, encode_png_rgb, require_solid_mask
from . import attention, hardware, large_model, memory, pipeline
from .components import (
    _SCHEDULER_MARKER,
    _SCHEDULER_SUBDIR,
    _TOKENIZER_MARKERS,
    _TOKENIZER_SUBDIR,
    _component_states,
    _first_unavailable_reason,
    _require_component_dir,
    checkpoint_is_distilled,
    component_dir_for_path,
    component_search_roots,
    is_gguf_file,
    require_encoder_transformer_compatible,
    require_text_encoder,
    text_encoder_available,
)
from .params import (
    _lenient_paths,
    _PATH_KEYS,
    _to_bool,
    derived_format,
    effective_steps,
    encoder_on_cpu,
    MIN_REGION_SIDE,
    normalize_flux2_klein_params,
    prompt_library_fields,
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
    _progress_reporter,
    FILE_COMPONENT_TEXT_ENCODER,
    FILE_COMPONENT_TRANSFORMER,
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
    LoadFrames,
    ProgressCb,
)
from .prompt_cache import (
    PROMPT_EMBED_CACHE_ENTRIES,
    Flux2PromptCacheAdapter,
    encoder_provenance,
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


def _load_frames(callback: ProgressCb | None, normalized: Mapping[str, Any]) -> LoadFrames | None:
    """The request's `LoadFrames` over `callback` (`None` without one): the transformer and the text encoder report their reads.

    `file_label` is the transformer's file or folder name and the encoder's
    GGUF file or transformers folder name (`_encoder_file_label`); the VAE
    reports no bytes.
    """
    if callback is None:
        return None
    files = {
        large_model.TRANSFORMER: (FILE_COMPONENT_TRANSFORMER, Path(normalized["transformer_path"]).name),
        large_model.TEXT_ENCODER: (FILE_COMPONENT_TEXT_ENCODER, _encoder_file_label(normalized)),
    }
    return LoadFrames(callback, files)


def _encoder_file_label(normalized: Mapping[str, Any]) -> str:
    """The `file_label` of the text encoder read: the GGUF file's name, else the transformers folder's name (a weights file inside it names its folder)."""
    source = Path(normalized["text_encoder_path"])
    if is_gguf_file(source):
        return source.name
    return component_dir_for_path(source).name


def _encoder_request(encoder_options: large_model.EncoderOptions) -> HandleRequest:
    """The text-encoder handle of a request for `paired`: its registration (by precision), `encoder_options` and their weight format."""
    return HandleRequest(large_model.encoder_model_name(encoder_options.fp8), encoder_options, large_model.weight_format_of(encoder_options))


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
        # Prompt embeddings in memory (`_memory_key` -> host tensor). Always
        # on, LRU, mutated only under `_lock`; its provenance index is replaced
        # wholesale on every change (copy-on-write) so the lock-free
        # `_prompt_plan` can iterate it. The orchestrator owns the plan and the
        # library surface over it.
        self._embeddings = EmbeddingMemory(PROMPT_EMBED_CACHE_ENTRIES)
        self._prompt_adapter = Flux2PromptCacheAdapter(self._effective_guidance_scale)
        self._prompts = PromptCacheOrchestrator(self._prompt_adapter, self._embeddings)
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
        """The pipeline's residency key for the transformer / VAE paths and `dtype` of `normalized` (a normalized request or the lenient status paths) on `device`.

        Only the key-bearing fields are filled: the key names the selected
        format's file, which is always `transformer_path`, so the format id
        (`transformer_format`, else derived from the container) never changes it.

        # Raises
        `ValueError` / `OSError` when the transformer file cannot be sniffed
        (`params.derived_format`); `_handle_key` turns that into "no key".
        """
        transformer_path = str(normalized["transformer_path"])
        transformer_format = normalized.get("transformer_format") or derived_format(transformer_path)
        options = large_model.PipelineOptions(
            transformer_path=transformer_path,
            vae_path=str(normalized["vae_path"]),
            dtype=normalized["dtype"],
            max_sequence_length=0,
            steps=1,
            guidance=False,
            text_attention_in_mask=False,
            vae_tiling=False,
            vae_tile_px=0,
            vae_scale=None,
            transformer_format=transformer_format,
        )
        return large_model.pipeline_spec(normalized["dtype"]).key(options, large_model.weight_format_of(options), normalize_device(device) or str(device))

    def _encoder_key(self, paths: Mapping[str, Any], fp8: bool, device: str, *, on_cpu: bool) -> str:
        options = large_model.EncoderOptions(str(paths.get("text_encoder_path") or ""), bool(fp8), 0, on_cpu=on_cpu)
        return large_model.encoder_spec(fp8).key(options, large_model.weight_format_of(options), normalize_device(device) or str(device))

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

    def _encoder_need(self, normalized: dict[str, Any], device: str, *, plan: PromptPlan | None = None) -> EncoderNeed:
        """THE decision of what the request in `normalized` asks of the text encoder on `device` (`EncoderNeed`).

        Used by `large_model_profiles` (whether the encoder is counted) and by
        the run (whether the encoder's combined ceiling is planned; it passes
        its own `plan`). Never waits for the service lock: the prompt plan
        reads the copy-on-write memory index and the library snapshot
        (`_prompt_plan`), and residency is read from the arbiter's ledger
        (`_holds`), like `health` and the status snapshot, so a forecast is
        answered during a running generation.
        """
        plan = plan if plan is not None else self._prompt_plan(normalized, blocking_load=True)
        encodes = text_encoder_available(normalized) and bool(plan.to_encode)
        resident = bool(normalized["text_encoder_path"]) and self._holds(self._handle_key(lambda: self._encoder_key(normalized, normalized["text_encoder_fp8"], device, on_cpu=normalized["encoder_on_cpu"]), "text_encoder"))
        return EncoderNeed(encodes=encodes, resident=resident)

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

    def _acquire(self, model: str, options: Any, shape: Any, steps: Steps, device: str, frames: LoadFrames | None, *, device_cap_bytes: int | None = None) -> Session:
        """Admit one session of `model` with `steps` (call under `self._lock`, pipeline before encoder: module notes).

        `frames`: the request's progress, whose `sink` becomes the session's
        load-progress sink — EVERY checkpoint read of the session (its loads,
        a DISK-tier component brought back inside a phase, a degrade retry's
        reload) streams its bytes on the current step (`LoadFrames`).
        `device_cap_bytes`: the per-handle device ceiling (`None` = none; the
        encoder's under the combined VRAM-axis rule, `paired` module notes).
        """
        session = self._large.acquire(model, options, shape, device=device, format=large_model.weight_format_of(options), steps=steps, observer=self._observer(model), device_cap_bytes=device_cap_bytes)
        if frames is not None:
            session.set_load_progress(frames.sink)
        return session

    def _combined_ceilings(self, alone: ProfilesReport, encoder_options: large_model.EncoderOptions, shape: RequestShape, device: str, *, until: VramStep) -> tuple[RamStep, dict[VramStep, int | None]] | None:
        """`(RAM step, combined ceilings)` of the pipeline report `alone`'s VRAM column for the encoder `encoder_options` (`paired.live_ceilings` over `memory.FLUX2_PAIR`).

        `None` when there is no VRAM axis to keep monotone. Call it BEFORE this
        run's pipeline holds a reservation (the manager would claim it a second
        time). Read-only.
        """
        return paired.live_ceilings(self._large, alone, _encoder_request(encoder_options), shape, device, memory.FLUX2_PAIR, until=until)

    @staticmethod
    def _encoder_cap(ceilings: tuple[RamStep, dict[VramStep, int | None]] | None, pipeline: Session) -> int | None:
        """The encoder's device ceiling beside the admitted `pipeline` session (`paired.encoder_cap_beside` over `memory.FLUX2_PAIR`); `None` without ceilings.

        A session admitted at another RAM step than the ceilings were planned
        for gets no ceiling, logged.
        """
        return paired.encoder_cap_beside(ceilings, pipeline, memory.FLUX2_PAIR, owner="FLUX.2 klein")

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
        reservation left. So (`paired.plan_pair` over `memory.FLUX2_PAIR`, its
        wire form `PairedReport.to_wire`): (1) the pipeline is planned alone;
        (2) the encoder is planned with that plan's reservation claimed, under
        the same requested `steps`, with the combined VRAM-axis ceiling a run
        would give it (`paired.live_ceilings`, `paired` module notes); (3) the
        pipeline is planned again with the encoder's reservation claimed, and
        that report is the answer (`ProfilesReport.to_dict()`; `model` is the
        dtype's registration):
        `selected` stays the PIPELINE's plan (the encoder enters it only as a
        claim), so the run's total is `selected` + `encoder`, summed by the
        client; each of the two `forecast` objects carries the additive
        `host_baseline` (the baseline RSS inside its `host_peak`,
        `paired.with_host_baseline`) so that sum counts the process once. The additive key `encoder` is the encoder's `selected`, or
        `null` when the encoder is not needed AND not resident
        (`EncoderNeed.counted`: every prompt the run needs is cached and the
        encoder handle holds no byte, or no encoder is installed); a resident
        encoder is counted even when the prompt is cached. `encoder_formats`
        is the encoder report's `formats` rows (one per `text_encoder_formats`
        candidate; `[]` when `encoder` is `null`). An encoder that does not
        fit makes `selected.fits` `false`, with the encoder's `reason` /
        `shortest_resource`. Read-only; never waits for the service lock.

        `format` is the transformer weight format to plan; `None` plans the
        request's selected one (`large_model.weight_format_of`), NOT the
        manager's best available format, so the answer describes the run the
        same params would start. The `formats` rows list every candidate of
        `transformer_formats`.

        # Raises
        `ValueError` for invalid params (or a format the model lacks).
        """
        normalized = normalize_flux2_klein_params(params)
        device = self._device_name()
        guidance = self._effective_guidance_scale(normalized) > 1.0
        options = large_model.pipeline_options(normalized, guidance=guidance)
        pipeline_request = HandleRequest(large_model.pipeline_model_name(normalized["dtype"]), options, format or large_model.weight_format_of(options))
        encoder_request = _encoder_request(large_model.encoder_options(normalized)) if self._encoder_need(normalized, device).counted else None
        return paired.plan_pair(self._large, pipeline_request, encoder_request, shape, device, steps, memory.FLUX2_PAIR).to_wire()

    # ---- status / health ----
    def status(self, params: dict[str, Any] | None = None) -> dict[str, Any]:
        """Component availability and residency for the given (or last) paths.

        `params` is read leniently: an absent or empty path is reported as "not
        configured". `prompt_cached` answers whether EVERY text the run needs
        is served by memory or the library (`_status_prompt_plan`), and
        `prompt_cache_entry` is the `EntryInfo` of the embedding that would
        serve the prompt (`null` when none); `text_encoder_available` whether an
        encoder is present on this machine; `guidance_supported` whether the
        guidance slider can do anything on this checkpoint. Never migrates v1
        entries.

        **Per-component residency** (`residency`, `actions`) comes from the
        large pool's ledger parts and is reported only when the service lock was
        free: a generation holds it for its entire run, so the probe never waits;
        when it is busy the two keys are absent and `components_busy` is true.
        **This call never blocks.** Free memory and forecasts are
        `large_models.profiles`' answer, never this one's.
        """
        paths = _lenient_paths(params) or dict(self._last_paths)
        components = _component_states(paths)
        prompt_cached, prompt_entry = self._status_prompt_cache(params)
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
            "prompt_cache_entry": prompt_entry,
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
        """Regenerate the masked part of `image_bytes` and return the RAW decoded region.

        `image_bytes` is the region PNG and `mask_bytes` the LATENT mask: an L8
        mask of the same size, already dilated by the client, where non-zero
        means "may be repainted". It is used as-is (no morphology here) as the
        pipeline's `mask_image` and as the text-attention target.
        `reference_bytes`, when given, is the user's marks over a copy of the
        region and must have EXACTLY the region's size (the pipeline's
        `image_reference`). The returned `image_png` is the RGB decode of the
        run, with no colour match and no composite: outside the mask it is a
        VAE round trip of the region, NOT byte-identical to it; the client owns
        every post-processing step. Under `whole_region` the mask must be solid.

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
        frames = _load_frames(progress_callback, normalized)
        report = _progress_reporter(frames, "load", LOAD_PHASE_STEPS)
        steps = memory.requested_steps(normalized)

        with self._lock:
            try:
                # The FIRST frame of the run, before any check: the checks read
                # headers and configs, which on a cold page cache take long
                # enough that the user would see nothing happen.
                report(LOAD_STEP_PREPARE, "Подготовка запуска FLUX.2 klein")
                self._prompt_adapter.migrate(normalized)
                # The plan is made ONCE, under the lock, and the run follows it
                # (`_prompt_embeds_locked` re-plans only when a chosen file vanished).
                plan = self._prompt_plan(normalized, blocking_load=True)
                if plan.to_encode:
                    require_text_encoder(normalized, what="генерация с этим промптом")
                # A 4B transformer beside a 9B encoder would otherwise die as a
                # bare matmul shape error inside the denoise.
                require_encoder_transformer_compatible(normalized)
                large_model.require_loadable(normalized, encoder=bool(plan.to_encode))
                result = self._run_locked(normalized, region_rgb, mask_u8, reference_rgb, steps, report, frames, plan)
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
        frames: LoadFrames | None,
        plan: PromptPlan,
    ) -> dict[str, Any]:
        """The run under `self._lock`: pipeline session, prompt, denoise, decode; answers the raw decode.

        `mask_u8` is the binarized latent mask from the wire, passed to the
        pipeline unchanged; `plan` the run's prompt plan (`_prompt_plan`).
        """
        height, width = region_rgb.shape[:2]
        device = self._device_name()
        guidance = self._effective_guidance_scale(normalized) > 1.0
        options = large_model.pipeline_options(normalized, guidance=guidance)
        shape = large_model.request_shape(width, height, with_reference=reference_rgb is not None, max_sequence_length=options.max_sequence_length, steps=options.steps)
        model = large_model.pipeline_model_name(normalized["dtype"])
        cold = not self._holds(self._handle_key(lambda: self._pipeline_key(normalized, device), "pipeline"))
        retries = 0
        # The encoder's combined ceiling is planned before the pipeline holds a
        # reservation (`_combined_ceilings`); only when this run will encode
        # (a resident encoder over a cached prompt is never touched by the run).
        ceilings = None
        if self._encoder_need(normalized, device, plan=plan).encodes:
            alone = self._large.profiles(model, options, shape, device=device, format=large_model.weight_format_of(options), steps=steps)
            ceilings = self._combined_ceilings(alone, large_model.encoder_options(normalized), shape, device, until=VramStep.MIN)
        session = self._acquire(model, options, shape, steps, device, frames)
        loaded = False
        try:
            with session:
                self._device = device
                self._active_key = session.key
                self._load_pipeline_locked(session, normalized, report, cold=cold)
                loaded = True
                embeds, encoder_retries = self._prompt_embeds_locked(normalized, plan, report, steps=steps, device=device, shape=shape, device_cap_bytes=self._encoder_cap(ceilings, session), frames=frames)
                retries += encoder_retries
                latents_cpu, image_processor = session.run_phase(
                    large_model.PHASE_DENOISE,
                    lambda ctx: self._denoise(ctx, region_rgb, mask_u8, normalized, embeds, frames, reference_rgb),
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
                f"{width}x{height}"
            )
        return {
            "image_png": encode_png_rgb(generated),
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
        *,
        cold: bool,
    ) -> None:
        """Bring the pipeline handle into its denoise state; on a `cold` handle report steps 1-6 and warm the VAE up.

        A cold handle held no byte when the run started: its components are
        read from disk here (their bytes reach the session's load-progress
        sink, `_acquire`), placed
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
        # The byte level of this read rides on this step (`LoadFrames`).
        report(LOAD_STEP_TRANSFORMER, "Загрузка трансформера")
        session.load(large_model.TRANSFORMER)
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

        The LATENT mask (`latent_mask`, the wire mask, dilated by the client)
        goes to the pipeline; with
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
        plan: PromptPlan,
        report: Callable[[int, str], None],
        *,
        steps: Steps,
        device: str,
        shape: Any,
        device_cap_bytes: int | None = None,
        frames: LoadFrames | None = None,
    ) -> tuple[dict[str, Any], int]:
        """`({"prompt", "negative"} host embeddings, encoder retries)` following `plan`.

        Served texts are taken from memory or read from their library file
        (which then stays in memory; `PromptCacheOrchestrator.serve_locked`);
        `plan.to_encode` is encoded. On a full
        hit no encoder is touched («Промпт взят из кэша»). A library file that
        vanished or changed since the plan (`LibraryEntryGone`) re-plans once;
        a text still unserved then is encoded when an encoder is available,
        else the run is refused naming the file. `negative` is `None` unless
        classifier-free guidance is active. `device_cap_bytes`: the encoder's
        device ceiling beside the pipeline (`_encoder_cap`); `frames` the
        request's progress (the encoder session's sink).

        # Raises
        `ValueError` when a chosen file vanished and nothing can replace it.
        """
        tensors: dict[str, Any] = {}
        lost: dict[str, str] = {}
        for text, choice in plan.served.items():
            tensor = self._prompts.serve_locked(choice)
            if tensor is None:
                lost[text] = choice.entry.file if isinstance(choice.entry, pl.LibraryEntry) else ""
            else:
                tensors[text] = tensor
        missing = list(plan.to_encode)
        if lost:
            replan = self._prompt_plan(normalized, blocking_load=True)
            for text in lost:
                choice = replan.served.get(text)
                tensor = self._prompts.serve_locked(choice) if choice is not None else None
                if tensor is None:
                    missing.append(text)
                else:
                    tensors[text] = tensor
            if missing and not text_encoder_available(normalized):
                files = ", ".join(sorted(file for text, file in lost.items() if text in missing and file)) or "—"
                raise ValueError(
                    f"Файл кэша промпта пропал или изменился во время запуска ({files}), а текстовый энкодер "
                    "недоступен — закодировать промпт заново нечем. Укажите текстовый энкодер или выберите "
                    "другой сохранённый промпт."
                )
        retries = 0
        if missing:
            retries, encoded = self._encode_prompts_locked(normalized, missing, report, frames, steps=steps, device=device, shape=shape, device_cap_bytes=device_cap_bytes)
            tensors.update(encoded)
        else:
            report(LOAD_STEP_ENCODER_DONE, "Промпт взят из кэша")
        embeds: dict[str, Any] = {"prompt": tensors[normalized["prompt"]], "negative": None}
        if self._effective_guidance_scale(normalized) > 1.0:
            embeds["negative"] = tensors[""]
        return embeds, retries

    def _encode_prompts_locked(
        self,
        normalized: dict[str, Any],
        texts: list[str],
        report: Callable[[int, str], None],
        frames: LoadFrames | None,
        *,
        steps: Steps,
        device: str,
        shape: Any,
        release: bool = False,
        device_cap_bytes: int | None = None,
    ) -> tuple[int, dict[str, Any]]:
        """Encode `texts` through an encoder session into the memory cache; returns `(retries, {text: embedding})`.

        THE place a missing encoder becomes fatal (`require_text_encoder`).
        Each embedding is remembered with the current encoder's provenance
        (`encoder_provenance`, computed before anything is loaded).
        Where the encoder computes is the plan's: the GPU when `steps` and the
        device allow it, else the CPU from RAM; between runs it stays as the
        steps keep it, unless `release` drops it after the encode (the
        prompt-cache build's own release).
        `device_cap_bytes` is the encoder's device ceiling beside an admitted
        pipeline (`None` when no pipeline is involved: the cache build).
        `frames` is the request's progress: the encoder's read streams its
        bytes on `LOAD_STEP_TEXT_ENCODER` (a read inside the encode phase, on
        the current step).
        """
        require_text_encoder(normalized, what="кодирование промпта")
        provenance = encoder_provenance(normalized)
        if provenance is None:
            # `require_text_encoder` just found the encoder: only a race with a deletion gets here.
            raise ValueError(f"Текстовый энкодер пропал: {normalized['text_encoder_path']}")
        from diffusers import Flux2KleinInpaintPipeline

        roots = component_search_roots(normalized)
        tokenizer = self._ensure_tokenizer_locked(_require_component_dir(roots, _TOKENIZER_SUBDIR, _TOKENIZER_MARKERS, "токенизатор Qwen"))
        options = large_model.encoder_options(normalized)
        model = large_model.encoder_model_name(options.fp8)
        msl = int(normalized["max_sequence_length"])
        report(LOAD_STEP_TEXT_ENCODER, "Загрузка текстового энкодера")
        encoder_session = self._acquire(model, options, shape, steps, device, frames, device_cap_bytes=device_cap_bytes)
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
        library = normalized.get("prompt_library") or ""
        created_at = pl.utc_now()
        encoded: dict[str, Any] = {}
        for text, embeds in zip(texts, results):
            self._prompts.remember_locked(library, text, msl, provenance, embeds, created_at)
            encoded[text] = embeds
        if release:
            self._unload_models_locked((model,))
        if self._holds(key):
            report(LOAD_STEP_ENCODER_DONE, "Текстовый энкодер оставлен в памяти")
        else:
            report(LOAD_STEP_ENCODER_DONE, "Текстовый энкодер выгружен")
        return retries, encoded

    # ---- prompt cache: build ----
    def prompt_cache_build(self, params: dict[str, Any] | None, *, name: str | None = None, progress_callback: ProgressCb | None = None) -> dict[str, Any]:
        """«Кэшировать»: save an entry of the CURRENT encoder for every text the run needs into the model's library, under the preset name `name`.

        The texts are the prompt plus `""` under effective classifier-free
        guidance. A text that already has an entry of the current encoder
        (same fingerprint and fp8 flag) is not written again; the others reuse
        an embedding this process encoded with that encoder, else they are
        encoded through the encoder handle alone (no pipeline is admitted;
        the encoder is released afterwards unless its handle already held
        bytes when the call arrived). Progress: step 0, then the prompt steps
        7-9 (step 7 streams the encoder read's bytes).

        **The name.** A preset is the library's entries of one exact prompt
        text; its name is stored in each entry. `name` empty or absent keeps the
        preset's current name (a new preset gets `default_preset_name`); a
        different name renames the preset — a header rewrite of all its
        entries, no encode (so "rename" is a build whose entries already exist).
        The empty negative prompt's entry is never named.

        Returns `{"prompt", "name", "encoded", "written": [EntryInfo],
        "renamed", "prompt_cached": true, "device"}`.

        # Raises
        `ValueError` for invalid params, an empty prompt, a request without
        `prompt_library`, no or an unidentifiable encoder, or a failed write;
        `LargeModelRefused` when the encode does not fit; whatever the encoder
        load raises.
        """
        normalized = normalize_flux2_klein_params(params)
        require_prompt_to_cache(normalized["prompt"])
        library = self._prompts.require_library(normalized)
        self._last_paths = {key: normalized[key] for key in _PATH_KEYS}
        frames = _load_frames(progress_callback, normalized)
        report = _progress_reporter(frames, "load", LOAD_PHASE_STEPS)

        def encode_missing(need: list[str]) -> dict[str, Any]:
            # Only reached when something must really be encoded: a build served
            # from memory needs no loadable encoder.
            large_model.require_loadable_encoder(normalized)
            report(LOAD_STEP_PREPARE, "Подготовка кэширования промпта")
            device = self._device_name()
            resident = self._holds(self._handle_key(lambda: self._encoder_key(normalized, normalized["text_encoder_fp8"], device, on_cpu=normalized["encoder_on_cpu"]), "text_encoder"))
            shape = large_model.request_shape(MIN_REGION_SIDE, MIN_REGION_SIDE, with_reference=False, max_sequence_length=int(normalized["max_sequence_length"]), steps=1)
            _retries, fresh = self._encode_prompts_locked(normalized, need, report, frames, steps=memory.requested_steps(normalized), device=device, shape=shape, release=not resident)
            return fresh

        def nothing_to_encode(any_to_write: bool) -> None:
            report(LOAD_STEP_ENCODER_DONE, "Промпт уже в кэше" if any_to_write else "Промпт уже в библиотеке")

        with self._lock:
            try:
                self._prompt_adapter.migrate(normalized)
                require_text_encoder(normalized, what="кэширование промпта")
                current = encoder_provenance(normalized)
                if current is None:
                    raise ValueError(f"Текстовый энкодер пропал: {normalized['text_encoder_path']}")
                built = self._prompts.build_locked(normalized, name, encode_missing, current=current, extra={"text_encoder_path": str(normalized["text_encoder_path"])}, on_nothing_to_encode=nothing_to_encode)
                self._last_error = None
                self._publish_health_locked()
            except Exception as exc:
                self._last_error = str(exc)
                self._publish_health_locked()
                raise
        log.info(
            "FLUX.2 klein: промпт «%s» в библиотеке %s: записано %d, %s, %s.",
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
            "written": [_entry_info(entry, built.current) for entry in built.written],
            "renamed": built.renamed,
            "prompt_cached": True,
            "device": self._device_label(),
        }

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
        frames = _load_frames(progress_callback, normalized)
        report = _progress_reporter(frames, "load", LOAD_PHASE_STEPS)
        if not self._lock.acquire(blocking=False):
            raise RuntimeError(_component_busy_message(component, action))
        try:
            try:
                self._require_action_available_locked(component, action, normalized)
                performed = self._component_action_locked(normalized, component, action, report, frames)
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
        frames: LoadFrames | None,
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
            with self._acquire(large_model.encoder_model_name(options.fp8), options, shape, steps, device, frames) as session:
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
                with self._acquire(model, options, shape, memory.requested_steps(normalized), device, frames) as session:
                    admitted_key = session.key
                    self._device = device
                    self._active_key = session.key
                    self._load_pipeline_locked(session, normalized, report, cold=True)
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
            with self._acquire(model, options, shape, _TO_RAM_STEPS if action == "to_ram" else _TO_GPU_STEPS, device, frames) as session:
                self._active_key = session.key
                if action == "to_gpu":
                    session.settle()
            return True
        # `warmup`: the VAE is its only component.
        with self._acquire(model, options, shape, memory.requested_steps(normalized), device, frames) as session:
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

    # ---- prompt library ----
    @staticmethod
    def _lenient_library_request(params: dict[str, Any] | None) -> dict[str, Any]:
        """What the browse-only library methods read from `params`, without requiring a transformer or a VAE.

        Returns `{prompt_library, text_encoder_source, prompt_cache_precision_order,
        text_encoder_path, text_encoder_format, text_encoder_fp8, transformer_path,
        max_sequence_length}`.

        # Raises
        `ValueError` when a prompt-library field breaks its contract.
        """
        raw = params if isinstance(params, dict) else {}
        library, source, order = prompt_library_fields(raw)
        paths = _lenient_paths(raw)
        fmt = raw.get("text_encoder_format")
        try:
            length = max(64, min(512, int(raw.get("max_sequence_length") or 512)))
        except (TypeError, ValueError):
            length = 512
        return {
            "prompt_library": library,
            "text_encoder_source": source,
            "prompt_cache_precision_order": order,
            "text_encoder_path": paths.get("text_encoder_path", ""),
            "text_encoder_format": fmt if isinstance(fmt, str) else "",
            "text_encoder_fp8": _to_bool(raw.get("text_encoder_fp8"), False),
            "transformer_path": paths.get("transformer_path", ""),
            "max_sequence_length": length,
        }

    def prompt_cache_list(self, params: dict[str, Any] | None) -> dict[str, Any]:
        """The presets of the request's library (`params.prompt_library`).

        Needs no transformer or VAE (lenient). Runs the one-time v1 migration
        first and reconciles the library against its files. One row per prompt
        text (the empty negative prompt is never listed), newest first:
        `{prompt, name, selected, entries}`, where `entries` are the text's
        entries at the request's `max_sequence_length` whose width the
        transformer accepts, best first by the selection rule, and `selected`
        the first of them (the entry a run would read when memory holds
        nothing better). A text without such entries is not listed. Every
        `EntryInfo` says whether the current encoder made it
        (`current_encoder`), so the client decides «Кэшировать» from it.

        Returns `{"library", "directory", "texts", "skipped": [{file, reason}],
        "text_encoder_available", "legacy": {migrated, deleted, not_migrated,
        directory} | null}`.

        # Raises
        `ValueError` for a request without `prompt_library` or a malformed
        library field.
        """
        return self._prompts.listing(self._lenient_library_request(params))

    def prompt_cache_export(self, params: dict[str, Any] | None, file: str, path: str) -> dict[str, Any]:
        """Copy the entry `file` of the request's library to the `.msprompt` path `path` (a byte copy, atomic).

        An existing destination is overwritten: the path comes from the
        client's save dialog, where "replace?" was already asked.

        Returns `{"library", "file", "path", "size_bytes"}`.

        # Raises
        `ValueError` for a request without `prompt_library`, a malformed file
        name, an entry that is not in the library, a source that is no longer a
        regular file inside the library (`pl.UnsafeLibraryPath`), or a rejected
        destination (`require_entry_destination`).
        """
        return self._prompts.export(self._lenient_library_request(params), file, path)

    def prompt_cache_import(self, params: dict[str, Any] | None, path: str) -> dict[str, Any]:
        """Copy an outside `.msprompt` file into the library it belongs to.

        A v2 file goes into ITS OWN library (its `library` field; it must be a
        FLUX.2 klein library), whatever library the request names:
        `library_matches` says whether the two agree. A v1 file (protocol <= 22)
        is converted by the migration rules (`classify_v1_entry`): a file made
        before the RoPE fix, or by an uncensored encoder that cannot be
        identified, or that cannot be migrated, is REFUSED naming the reason.
        The tensor bytes are kept verbatim; an entry that is already present is
        not written again (`already_present`).

        Returns `{"library", "file", "prompt", "name", "library_matches",
        "already_present", "origin"}`.

        # Raises
        `ValueError` for a rejected path, a file that is not a prompt cache, a
        library that is not FLUX.2's, a refused v1 file, or a failed write.
        """
        return self._prompts.import_file(self._lenient_library_request(params), path)

    # ---- the prompt plan ----
    def _prompt_plan(self, normalized: dict[str, Any], *, blocking_load: bool) -> PromptPlan:
        """THE owner of "which embedding serves each text this run needs, and what must be encoded" (`PromptCacheOrchestrator.plan`).

        The texts are the prompt plus `""` under effective classifier-free
        guidance (`Flux2PromptCacheAdapter.texts_needed`); candidates are the
        memory embeddings and the library entries of the request's library at
        its `max_sequence_length` that the transformer's width accepts
        (`transformer_text_width`), ranked by `prompt_library.select_entry`.
        Lock-free; never migrates.
        """
        return self._prompts.plan(normalized, blocking_load=blocking_load)

    def _status_prompt_cache(self, params: dict[str, Any] | None) -> tuple[bool, dict[str, Any] | None]:
        """`(prompt_cached, prompt_cache_entry)` of `.status`, read leniently.

        An empty prompt, a missing path or an invalid field answers
        `(False, None)` instead of raising: the UI polls this while the user is
        still typing. `whole_region` is cleared before normalizing (it cannot
        change the plan, and clearing it keeps `_whole_region_overrides`'s log
        line out of a polling path). Never takes the service lock and never
        migrates.
        """
        if not isinstance(params, dict) or not str(params.get("prompt", "") or "").strip():
            return False, None
        probe = dict(params)
        probe["whole_region"] = False
        try:
            normalized = normalize_flux2_klein_params(probe)
        except ValueError:
            return False, None
        return self._prompts.status(normalized)

    def _prompt_cached(self, params: dict[str, Any] | None) -> bool:
        """Whether every text a run of `params` needs is served by memory or the library (`_status_prompt_cache`, lenient)."""
        return self._status_prompt_cache(params)[0]

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

    def _store_embeds(self, remembered: MemoryEmbedding, value: Any) -> None:
        """Insert into the LRU memory cache (caller holds `self._lock`), evicting the oldest entry when full (`EmbeddingMemory.store`; the test fixtures' seam)."""
        self._embeddings.store(remembered, value)

    @property
    def _memory_index(self) -> Mapping[tuple[Any, ...], MemoryEmbedding]:
        """The memory cache's provenance index (copy-on-write; read by the prompt-cache golden test)."""
        return self._embeddings.index

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

        `latent_mask` is the wire's latent mask (dilated by the client) that the
        pipeline receives as `mask_image`,
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
