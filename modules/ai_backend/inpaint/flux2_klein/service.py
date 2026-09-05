"""
File: modules/ai_backend/inpaint/flux2_klein/service.py

Purpose:
`Flux2KleinInpaintService` - the object `server.py` puts on `AppState` and every
`inpaint.flux2_klein.*` IPC handler calls. It owns the service lock, the resident
pipeline, the in-memory prompt-embedding cache and the shared model lease; the
work itself is delegated to the sibling modules of this package.

Main responsibilities:
- `status` / `estimate` / `health` / `unload` - what the client polls;
- `inpaint_image_bytes` - one region edit, in the documented order: transformer +
  VAE are loaded, placed and warmed up FIRST, then the 16 GB text encoder is read
  into the host memory the transformer has just vacated;
- `component_action` - one per-component residency action under the same memory
  guard and lease protocol a generation uses;
- the `prompt_cache_*` methods - the library surface, keyed identically to the
  in-memory cache through `_prompt_cache_key`.

Notes:
- Load and inference are two separate `try` scopes: see the lease-protocol
  section of `modules/ai_backend/inpaint/MODULE_README.md`.
- Symbols the test suite monkeypatches are reached through their defining module
  (`hardware.memory_snapshot`, `pipeline._restore_transformer_to_device`,
  `prompt_cache.write_prompt_file`, ...) so that one patch reaches every caller.
- torch / diffusers / transformers / cv2 are imported lazily inside the methods
  that need them.
"""

from __future__ import annotations

import logging
import os
import threading
import time
from collections import OrderedDict
from pathlib import Path
from typing import TYPE_CHECKING, Any, Callable

if TYPE_CHECKING:
    import numpy as np

from ...runtime.model_manager import LoadedModelManager
from . import hardware, pipeline, prompt_cache
from .components import (
    _SCHEDULER_MARKER,
    _SCHEDULER_SUBDIR,
    _TOKENIZER_MARKERS,
    _TOKENIZER_SUBDIR,
    _component_states,
    _first_unavailable_reason,
    _require_component_dir,
    component_search_roots,
    require_encoder_transformer_compatible,
    require_text_encoder,
    text_encoder_available,
)
from .imaging import (
    _composite_over_region,
    _decode_image_rgb,
    _decode_mask,
    _dilate_mask,
    _encode_png_bytes_rgb,
    _match_color_outside_mask,
    _require_solid_mask,
)
from .memory import _fits, _require_memory_headroom, forecast_memory
from .params import (
    _GPU_ONLY_PLACEMENTS,
    _lenient_paths,
    _model_key,
    _PATH_KEYS,
    effective_steps,
    MIN_REGION_SIDE,
    normalize_flux2_klein_params,
    validate_region_size,
)
from .pipeline import (
    _apply_placement,
    _apply_vae_memory_options,
    _component_actions,
    _component_busy_message,
    _decode_region_latents,
    _encode_prompt_phase,
    _LEASED_ACTIONS,
    _load_text_encoder,
    _load_transformer,
    _load_vae,
    _module_residency,
    _park_transformer_off_device,
    _require_action_name,
    _require_component_name,
    _require_components_materialized,
    _require_execution_device,
    _warmup_vae_decode,
    ACTIONABLE_COMPONENTS,
)
from .progress import (
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

# =====================================================================
#  Service
# =====================================================================
class Flux2KleinInpaintService:
    """Lazy-loading FLUX.2 klein region editor for `inpaint.flux2_klein`.

    One pipeline is resident at a time, guarded by an `RLock` and leased from the
    shared `LoadedModelManager` under a key derived from the three user paths and
    the placement/dtype choice.
    """

    def __init__(self, model_manager: LoadedModelManager) -> None:
        self._lock = threading.RLock()
        self._model_manager = model_manager
        self._pipe: Any = None
        self._active_key: str | None = None
        self._device: Any = None
        self._last_error: str | None = None
        # Whether the pipeline in `self._pipe` has been warmed up SINCE its
        # weights were last placed. The warm-up is a property of a PLACEMENT, not
        # of a request: everything it proves (nothing left on `meta`, the queued
        # host->device copies retired, the host pages released before the 16 GB
        # encoder is read) is established once and stays true until something
        # moves the weights again. See `_warmup_pipeline_if_needed_locked` for the
        # exhaustive list of what clears it.
        self._pipeline_warmed = False
        # The Qwen tokenizer, with the directory it was read from. It is a few MB
        # of vocabulary and it is needed on every prompt encode as well as by the
        # pipeline itself, so it is read ONCE per directory instead of once per
        # encode — on a model that lives on a spinning disk that read is real I/O
        # on an otherwise hot pipeline. Invalidated when the discovered directory
        # changes; dropped by `unload()`.
        self._tokenizer: Any = None
        self._tokenizer_dir: str | None = None
        # Paths of the last accepted request, so `status()` can report component
        # state without being handed the params again.
        self._last_paths: dict[str, str] = {}
        # Phase-1 results, keyed by everything that changes them
        # (`_prompt_cache_key`). Always on, LRU, bounded by
        # `PROMPT_EMBED_CACHE_ENTRIES`: a mask edit, a new seed or a repeated
        # prompt must not re-read 16 GB of encoder from disk. Entries live on the
        # HOST, so the cache never pins device memory.
        self._prompt_cache: OrderedDict[tuple[Any, ...], Any] = OrderedDict()
        # The text encoder between runs, when the user asked to keep it
        # (`unload_text_encoder_after_encode=False`). It is NOT part of the
        # pipeline object and NOT part of `_active_key` unless it is resident —
        # see `_model_key`.
        self._text_encoder: Any = None
        self._text_encoder_key: tuple[Any, ...] | None = None

    # ---- status / health ----
    def status(self, params: dict[str, Any] | None = None) -> dict[str, Any]:
        """Component availability and free memory for the given (or last) paths.

        `params` is optional and is read leniently: an absent or empty path is
        reported as "not configured" rather than raising, because the UI calls
        this before the user has finished choosing files.

        `prompt_cached` answers whether a READY embedding exists for the exact
        combination in `params` (prompt + encoder + `max_sequence_length` +
        dtype + fp8); see `_prompt_cached`.

        `text_encoder_available` says whether an encoder is present ON THIS
        MACHINE. It is reported separately from `available` because the two
        answer different questions: a run whose prompt is already cached needs no
        encoder at all, so `available` stays `true` while this flag is `false`,
        and the client is expected to warn that only ready caches will work —
        `prompt_cache.build` and any new prompt are refused until an encoder is
        configured.

        **Per-component residency.** The three weight-bearing entries of
        `components` also carry `residency` (a `RESIDENCY_*` literal) and
        `actions` (what the client may offer right now, see
        `_component_actions`) — but only when the service lock was free. A
        generation holds that lock for its ENTIRE run, so the probe takes it
        WITHOUT waiting: otherwise this poll would block the handler for minutes.
        When it is busy the two keys are simply absent and `components_busy` is
        `true`; an absent key means "not known" and never "not loaded", the same
        three-state rule the client already applies to `prompt_cached`.

        **This call never blocks.** Everything else it reports is read without
        the lock: `self._pipe` / `self._device` are single attribute loads and a
        prompt-cache membership test is one dict lookup, all atomic under the
        GIL, with no compound invariant for a lock to protect. Taking the lock
        for them made a status poll wait out a whole generation.
        """
        paths = _lenient_paths(params) or dict(self._last_paths)
        components = _component_states(paths)
        prompt_cached = self._prompt_cached(params)
        reason = _first_unavailable_reason(components, prompt_cached=prompt_cached)
        residency, busy = self._components_residency_nowait()
        if residency is not None:
            for name, state in residency.items():
                components[name].update(state)
        device = self._device_label()
        return {
            "available": reason is None,
            "reason": reason,
            "components": components,
            "components_busy": busy,
            "memory": hardware.memory_snapshot(device),
            "loaded": self._pipe is not None,
            "device": device,
            "prompt_cached": prompt_cached,
            "text_encoder_available": text_encoder_available(paths),
        }

    def _components_residency_nowait(self) -> tuple[dict[str, dict[str, Any]] | None, bool]:
        """`(per-component residency + actions, busy)` without ever waiting.

        Returns `(None, True)` when the service lock is held — by a generation,
        by a component action or by a prompt-cache build. The probe itself must
        run under the lock: it iterates a module's `parameters()` while a
        concurrent load may still be moving them.
        """
        if not self._lock.acquire(blocking=False):
            return None, True
        try:
            return self._components_locked(), False
        finally:
            self._lock.release()

    def _components_locked(self) -> dict[str, dict[str, Any]]:
        """Residency and available actions of the three weight-bearing components.

        Caller must hold `self._lock`. `pipeline_loaded` is passed to
        `_component_actions` rather than derived from each component's own
        residency, so the "load and unload act on the pipeline as a whole"
        invariant holds structurally instead of by coincidence.
        """
        pipeline_loaded = self._pipe is not None
        modules = {
            "text_encoder": self._text_encoder,
            "transformer": getattr(self._pipe, "transformer", None),
            "vae": getattr(self._pipe, "vae", None),
        }
        out: dict[str, dict[str, Any]] = {}
        for name in ACTIONABLE_COMPONENTS:
            residency = _module_residency(modules[name])
            out[name] = {
                "residency": residency,
                "actions": _component_actions(name, residency, pipeline_loaded=pipeline_loaded),
            }
        return out

    def _device_label(self) -> str:
        """The device this service runs on: the loaded one, else the planned one.

        Before the first load `self._device` is empty, and answering `"cpu"`
        there is a lie with consequences — it tells the user that a run costing
        tens of minutes will happen on the CPU while it will in fact happen on
        the accelerator selected in `General.ai_device`. The planned value comes
        from the same `_resolve_selected_backend_device("cuda")` the pipeline
        build uses, so the two cannot disagree. Callers pair it with
        `loaded` / `ready`, which say whether the answer is a fact or a plan.

        Takes NO lock. `self._device` is a single attribute load — atomic under
        the GIL, with no compound invariant a lock could protect — and this
        method is on `status()`'s path, which must answer while a generation
        holds the lock for its entire run instead of waiting minutes for it.
        """
        device = self._device
        if device is not None:
            return str(device)
        return hardware._resolve_selected_backend_device("cuda")

    def estimate(
        self,
        *,
        params: dict[str, Any] | None,
        region_width: int,
        region_height: int,
    ) -> dict[str, Any]:
        """Forecast the RAM/VRAM cost of one run with `params` on that region.

        The arithmetic lives in `forecast_memory`, which is also what the
        pre-load memory guard uses — the UI's advice and the guard's refusal must
        never be two independent calculations. `fits` compares the forecast
        against the currently free memory and is `True` when a side is unknown;
        HERE it is advice for the UI, while `_require_memory_headroom` is the
        gate. The free VRAM is read from the accelerator this service would
        actually use, so a forecast on a two-GPU host is not compared against the
        wrong card.

        # Raises
        `ValueError` for invalid params (see `normalize_flux2_klein_params`) or
        an invalid region (see `validate_region_size`).
        """
        normalized = normalize_flux2_klein_params(params)
        validate_region_size(region_width, region_height)

        forecast = forecast_memory(normalized, region_width, region_height)
        memory = hardware.memory_snapshot(self._device_label())
        return {
            "vram_bytes": forecast["vram_bytes"],
            "ram_bytes": forecast["ram_bytes"],
            "vram_free": int(memory["vram_free"]),
            "ram_free": int(memory["ram_free"]),
            "fits": _fits(forecast["vram_bytes"], memory["vram_free"])
            and _fits(forecast["ram_bytes"], memory["ram_free"]),
            "breakdown": forecast["breakdown"],
        }

    def health(self) -> dict[str, Any]:
        """Snapshot for the periodic backend health event.

        `device` follows `_device_label`: the loaded device, or the one that
        would be used, never a placeholder. `ready` tells the two apart.
        """
        device = self._device_label()
        with self._lock:
            return {
                "ready": self._pipe is not None,
                "model": "flux2_klein",
                "device": device,
                "active_key": self._active_key,
                "last_error": self._last_error,
            }

    def unload(self) -> bool:
        """Drop the resident pipeline and text encoder; `False` when nothing was loaded.

        The prompt cache is deliberately KEPT: it holds a few MB of embeddings,
        not weights, and dropping it would make the next run re-read 16 GB of
        encoder for a prompt that has not changed. The cached tokenizer IS
        dropped: this method is also the eviction callback's route
        (`_unload_key`), and an eviction exists to give memory back, so nothing
        this service allocated should survive it.
        """
        with self._lock:
            had_encoder = self._text_encoder is not None
            self._release_text_encoder_locked()
            self._tokenizer = None
            self._tokenizer_dir = None
            return self._unload_pipeline_locked() or had_encoder

    def _unload_pipeline_locked(self) -> bool:
        """Drop the resident pipeline; `False` when none was loaded.

        Caller must hold `self._lock`. The text encoder is deliberately NOT
        touched: it is not a pipeline component here (`_ensure_pipeline_locked`
        builds the pipeline with `text_encoder=None`) and the per-component
        «unload» of the transformer or the VAE must not silently cost the user a
        16 GB re-read of the encoder as well. `unload()` drops both, which is
        what the `.unload` wire method promises.
        """
        if self._pipe is None:
            return False
        key = self._active_key
        self._pipe = None
        self._active_key = None
        # The weights are gone, so the warm-up that proved where they sat is
        # meaningless; the pipeline built next owes a fresh one.
        self._pipeline_warmed = False
        hardware._clear_torch_cache()
        if key is not None:
            self._model_manager.mark_unloaded(key)
        return True

    # ---- main entry ----
    def inpaint_image_bytes(
        self,
        image_bytes: bytes,
        mask_bytes: bytes,
        *,
        params: dict[str, Any] | None = None,
        progress_callback: ProgressCb | None = None,
    ) -> dict[str, Any]:
        """Regenerate the masked part of `image_bytes` and composite it back.

        `image_bytes` is the region PNG and `mask_bytes` an L8 mask of exactly the
        same size, where non-zero means "may change". The returned `image_png` has
        the region's size and is byte-identical to the input outside the mask.
        Under `whole_region` the mask must be solid (every pixel 255) and the
        whole region is regenerated; a non-solid mask there is a request error,
        not a silently narrowed edit.

        **The order of the two phases is a memory contract.** The transformer and
        the VAE are loaded and placed FIRST and then warmed up, so their weights
        are provably on the accelerator; only then is the 16 GB text encoder read,
        into a host that the transformer has just left. It encodes the prompt into
        a few MB of embeddings and stays in host memory for the next prompt. The
        reverse order — the one this replaced — made the encoder's host peak and
        the transformer's host peak overlap, which is what a run has to avoid on a
        machine whose host memory is smaller than their sum.

        The warm-up is owed once per PLACEMENT, not once per request: a run that
        cache-hits an already-warm pipeline skips it entirely
        (`_warmup_pipeline_if_needed_locked`), because nothing it proves can have
        stopped being true while no weight moved.

        # Raises
        `ValueError` for bad params, a bad region size, a mask size mismatch, a
        non-solid mask under `whole_region`, or a text encoder whose width does
        not match the transformer (`require_encoder_transformer_compatible`);
        `FileNotFoundError` when a component is missing; `RuntimeError` when a
        phase does not fit in the free memory; whatever the pipeline raises
        during generation.
        """
        normalized = normalize_flux2_klein_params(params)
        region_rgb = _decode_image_rgb(image_bytes)
        height, width = region_rgb.shape[:2]
        validate_region_size(width, height)
        mask_u8 = _decode_mask(mask_bytes, expected_hw=(height, width))
        if normalized["whole_region"]:
            _require_solid_mask(mask_u8)
        self._last_paths = {key: normalized[key] for key in _PATH_KEYS}

        model_key = _model_key(normalized)
        lease = self._model_manager.begin_model_use(
            model_key, unload_callback=lambda: self._unload_key(model_key)
        )
        report = _progress_reporter(progress_callback, "load", LOAD_PHASE_STEPS)
        with self._lock:
            try:
                # Load scope: only a failure in here is a failed LOAD, and only
                # then may the manager drop its entry for `model_key`. It now ends
                # at `_ensure_pipeline_locked`, because everything after it runs
                # with the pipeline already resident — see below.
                try:
                    # The FIRST frame of the run, before any guard: both the
                    # encoder check and the memory guard touch the filesystem, and
                    # on a cold page cache they take long enough that the user
                    # sees nothing happen after pressing the button. The frame
                    # costs nothing and it is the step this scale already calls
                    # «preparation», so it belongs at the very start of the scope
                    # it names, not after the guards.
                    report(LOAD_STEP_PREPARE, "Подготовка запуска FLUX.2 klein")
                    # Before anything is read: a prompt that is not in the cache
                    # needs the encoder, and on a machine that has none the run
                    # cannot succeed. `_encode_prompts_locked` would refuse it
                    # too, but only after 18 GB of transformer had been loaded
                    # and placed for nothing.
                    if self._prompts_to_encode(normalized):
                        require_text_encoder(normalized, what="генерация с этим промптом")
                    # Two `config.json` reads, no weight: a 4B transformer beside
                    # a 9B encoder otherwise passes every check here, passes the
                    # memory guard, and dies as a bare matmul shape error inside
                    # the denoise — after ~34 GB has been read from disk.
                    require_encoder_transformer_compatible(normalized)
                    self._require_headroom_locked(normalized, width, height, model_key)
                    pipe = self._ensure_pipeline_locked(
                        normalized, model_key, report, region_hw=(height, width)
                    )
                except Exception:
                    if lease.needs_load:
                        lease.mark_load_failed()
                    raise
                # The pipeline is resident from here on, so it is registered
                # before ANYTHING else: the warm-up and the prompt phase can both
                # fail, and a failure there is a failed run, not a failed load.
                # Reporting it as a failed load would clear the manager's
                # `resident` flag and drop the unload callback while the 9B
                # transformer still occupies VRAM — see the lease-protocol
                # section of `inpaint/MODULE_README.md`.
                if lease.needs_load:
                    lease.mark_loaded(unload_callback=lambda: self._unload_key(model_key))
                self._warmup_pipeline_if_needed_locked(pipe, normalized, report)
                embeds = self._prompt_embeds_locked(normalized, report)
                out_rgb, applied, oom_recovered = self._generate_locked(
                    pipe, region_rgb, mask_u8, normalized, embeds, progress_callback
                )
                self._last_error = None
            except Exception as exc:
                self._last_error = str(exc)
                raise
            finally:
                lease.release()

        return {
            "image_png": _encode_png_bytes_rgb(out_rgb),
            "region_size": [int(width), int(height)],
            # The pipeline is loaded at this point, so this is the device the run
            # actually happened on, not a plan.
            "device": self._device_label(),
            "placement": normalized["placement"],
            # The settings actually in force after any OOM recovery, so the Rust
            # side can persist them and take the cheap path next time.
            "applied": applied,
            "oom_recovered": oom_recovered,
        }

    # ---- prompt cache: build / save / load ----
    def prompt_cache_build(
        self,
        params: dict[str, Any] | None,
        *,
        progress_callback: ProgressCb | None = None,
    ) -> dict[str, Any]:
        """Encode the prompt into the cache WITHOUT generating anything.

        This is the "Кэшировать" button: it loads the text encoder, encodes the
        prompt into the very cache a generation reads from (`_prompt_cache`,
        keyed by `_prompt_cache_key` — there is no second cache and no second
        key), and then lets the encoder go again. Nothing else is loaded: the
        transformer and the VAE take no part in a prompt, so the memory guard
        checks the `encode_standalone` phase only and this call never demands the
        18 GB a run would.

        **The encoder is released afterwards**, because releasing it is the point
        of the button: a user caches a prompt precisely so that the next runs can
        happen without 16 GB of Qwen3 resident. The one exception is an encoder
        that was ALREADY resident with the same key when the call arrived — that
        one belongs to the previous run's settings, and dropping it here would
        cost that user a 16 GB re-read they never asked for, so their own
        `unload_text_encoder_after_encode` decides.

        Progress is the shared `phase:"load"` scale: step 0, then the prompt
        phase's own steps 7-9. Steps 1-6 belong to the pipeline, which this call
        does not build, so they are simply never emitted.

        **No model-manager lease is taken**, and that is consistent rather than
        an omission: a lease exists to account for what stays RESIDENT, and this
        call leaves nothing new behind (see the paragraph above). An encoder it
        keeps because the call found it there was already covered by the lease of
        the run that loaded it.

        Returns `{"prompt", "encoded", "prompt_cached", "device"}`; `encoded` is
        `False` when the cache already covered the prompt and nothing was read.

        # Raises
        `ValueError` for invalid params or an empty prompt; `RuntimeError` when
        the encode phase does not fit in the free host memory; whatever the
        encoder loader raises.
        """
        normalized = normalize_flux2_klein_params(params)
        text = normalized["prompt"]
        if not text:
            raise ValueError(
                "Кэшировать нечего: промпт пуст. Введите текст промпта и повторите."
            )
        self._last_paths = {key: normalized[key] for key in _PATH_KEYS}
        report = _progress_reporter(progress_callback, "load", LOAD_PHASE_STEPS)

        with self._lock:
            try:
                if self._prompt_cache_key(normalized, text) in self._prompt_cache:
                    report(LOAD_STEP_ENCODER_DONE, "Промпт уже в кэше")
                    encoded = False
                else:
                    require_text_encoder(normalized, what="кэширование промпта")
                    self._require_encode_headroom_locked(normalized)
                    report(LOAD_STEP_PREPARE, "Подготовка кэширования промпта")
                    self._encode_prompts_locked(
                        self._build_encode_params_locked(normalized), [text], report
                    )
                    encoded = True
                self._last_error = None
            except Exception as exc:
                self._last_error = str(exc)
                raise

        log.info(
            "FLUX.2 klein: кэш промпта готов (%s), длина промпта %d символов.",
            "закодирован" if encoded else "уже был в памяти",
            len(text),
        )
        return {
            "prompt": text,
            "encoded": encoded,
            "prompt_cached": True,
            "device": self._device_label(),
        }

    def _build_encode_params_locked(self, normalized: dict[str, Any]) -> dict[str, Any]:
        """`normalized` with the encoder-release flag `prompt_cache_build` needs.

        Caller must hold `self._lock`. The flag is not part of
        `_prompt_cache_key` nor of `_encoder_key`, so overriding it changes only
        what happens to the encoder AFTER the encode — see `prompt_cache_build`
        for why an already-resident encoder is left to the user's own setting.
        """
        resident = (
            self._text_encoder is not None
            and self._text_encoder_key == self._encoder_key(normalized, "cpu")
        )
        if resident:
            return normalized
        params = dict(normalized)
        params["unload_text_encoder_after_encode"] = True
        return params

    def _require_encode_headroom_locked(self, normalized: dict[str, Any]) -> None:
        """Gate a standalone prompt encode on the free host memory.

        Caller must hold `self._lock`. Only the `encode_standalone` phase is
        checked: `prompt_cache_build` reads the text encoder and nothing else, so
        demanding room for the transformer would refuse a request that fits. The
        region size is passed because `forecast_memory` takes one, but this phase
        does not depend on it — no latent and no pixel is produced here — so the
        smallest valid region is used.
        """
        encoder_resident = (
            self._text_encoder is not None
            and self._text_encoder_key == self._encoder_key(normalized, "cpu")
        )
        _require_memory_headroom(
            normalized,
            MIN_REGION_SIDE,
            MIN_REGION_SIDE,
            hardware._resolve_selected_backend_device("cuda"),
            phases=("encode_standalone",),
            # The pipeline takes no part in this phase, so there is nothing of it
            # to discount: `encode_standalone` charges neither its VRAM nor its
            # host copy.
            pipeline_resident=False,
            encoder_resident=encoder_resident,
        )

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

        `component` is one of `ACTIONABLE_COMPONENTS` and `action` one of
        `COMPONENT_ACTIONS`; the pair must be present in that component's CURRENT
        `actions` list (`_component_actions`), which is re-checked here under the
        lock rather than trusted from whatever snapshot the client last saw.

        Streaming, like `prompt_cache.build`: reading the encoder takes ~106 s
        and the transformer 18 GB, so progress is reported on the shared
        `phase:"load"` scale and this call claims the same single progress bar a
        generation does.

        Returns `{"component", "action", "performed", "components",
        "components_busy": False, "device"}`. `performed` is `False` for an
        action that turned out to be a no-op (a warm-up the placement skips).

        **Refusals are errors, never silent no-ops**, because the button the user
        pressed either happened or must say why it did not:

        - `ValueError` — unknown component or action, invalid params, or an
          action that is not in that component's current `actions`;
        - `RuntimeError` — the service is busy (a generation holds the lock for
          its entire run, and this call takes it WITHOUT waiting rather than
          hanging the handler for minutes), or the pre-load memory guard refuses;
        - `FileNotFoundError` — a component path is gone.

        The lease protocol is the one every other path here follows: the lease is
        taken BEFORE `self._lock`, only the action that can make a new key
        resident takes one at all (`_LEASED_ACTIONS`), and `mark_load_failed` is
        reported from the load scope only — see `_load_pipeline_action_locked`.
        The `finally` additionally resolves a lease no load scope ever reached,
        so a refusal cannot leave a manager entry stuck in `loading`.
        """
        component = _require_component_name(component)
        action = _require_action_name(action)
        normalized = normalize_flux2_klein_params(params)
        self._last_paths = {key: normalized[key] for key in _PATH_KEYS}
        report = _progress_reporter(progress_callback, "load", LOAD_PHASE_STEPS)
        model_key = _model_key(normalized)

        # Advisory busy check, deliberately BEFORE the lease. `begin_model_use`
        # waits on `_condition` while another thread is loading the same key, and
        # a generation holds `self._lock` across its whole load — so without this
        # the busy refusal this method owes the user would instead become a
        # multi-minute wait inside the model manager. It is racy by construction
        # (a generation can start in the window below); the authoritative check is
        # the non-blocking acquire further down, which only ever turns the race
        # into a late refusal, never into a wrong answer.
        self._refuse_when_busy(component, action)

        lease = None
        if action in _LEASED_ACTIONS:
            lease = self._model_manager.begin_model_use(
                model_key, unload_callback=lambda: self._unload_key(model_key)
            )
        try:
            if not self._lock.acquire(blocking=False):
                raise RuntimeError(_component_busy_message(component, action))
            try:
                try:
                    self._require_action_available_locked(component, action)
                    performed = self._component_action_locked(
                        normalized, model_key, component, action, report, lease
                    )
                except Exception as exc:
                    self._last_error = str(exc)
                    raise
                self._last_error = None
                components = self._components_locked()
            finally:
                self._lock.release()
        finally:
            if lease is not None:
                # Resolve the lease before releasing it, on EVERY path. A lease
                # taken with `needs_load` that is released without either
                # `mark_loaded` or `mark_load_failed` leaves its manager entry
                # flagged `loading` forever, and the next `begin_model_use` for
                # that key then waits on a load that will never finish — which is
                # what the busy refusal and the "action no longer available"
                # refusal above would otherwise do, both of which happen after
                # the lease is taken. `mark_load_failed()` is idempotent and a
                # strict no-op once the load has been reported either way, so it
                # cannot undo the `mark_loaded` of a successful load whose
                # WARM-UP then failed (the lease-protocol contract).
                lease.mark_load_failed()
                lease.release()

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

    def _refuse_when_busy(self, component: str, action: str) -> None:
        """Raise the busy refusal when the service lock is not free right now.

        Advisory only — see the comment at its call site in `component_action`.
        The lock is released immediately, so nothing is held across the
        `begin_model_use` that follows; holding it there is the deadlock the
        lease protocol exists to prevent.
        """
        if not self._lock.acquire(blocking=False):
            raise RuntimeError(_component_busy_message(component, action))
        self._lock.release()

    def _require_action_available_locked(self, component: str, action: str) -> None:
        """Refuse an action that is not in `component`'s current `actions` list.

        Caller must hold `self._lock`. The message names what IS possible right
        now, because the usual cause is a client acting on a snapshot that a
        generation, an eviction or another action has since invalidated.

        # Raises
        `ValueError` naming the component, the refused action and the ones that
        are available.
        """
        available = self._components_locked()[component]["actions"]
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
        model_key: str,
        component: str,
        action: str,
        report: Callable[[int, str], None],
        lease: Any,
    ) -> bool:
        """Dispatch one already-validated action; `True` when something changed.

        Caller must hold `self._lock`, and `lease` is the model-manager lease for
        the actions that take one (`None` otherwise). Every branch reuses the
        helper the generation path already uses — nothing here is a second
        implementation of a move, a load or a warm-up.
        """
        if action == "load":
            if component == "text_encoder":
                return self._load_text_encoder_action_locked(normalized, report)
            return self._load_pipeline_action_locked(normalized, model_key, report, lease)
        if action == "unload":
            if component == "text_encoder":
                released = self._text_encoder is not None
                self._release_text_encoder_locked()
                return released
            return self._unload_pipeline_locked()
        if action == "to_ram":
            report(LOAD_STEP_PLACEMENT, "Выгрузка трансформера в оперативную память")
            moved = _park_transformer_off_device(self._pipe, normalized["placement"])
            if moved:
                # Weights left the device, so the placement this pipeline was
                # warmed for no longer describes it.
                self._pipeline_warmed = False
            return moved
        if action == "to_gpu":
            return self._restore_transformer_action_locked(normalized, report)
        # `warmup`: the only remaining action, and the VAE is its only component.
        # The unconditional primitive on purpose — the user pressed the button, so
        # the pass must happen even when the pipeline is already marked warm.
        return self._warmup_pipeline_locked(self._pipe, normalized, report)

    def _load_text_encoder_action_locked(
        self, normalized: dict[str, Any], report: Callable[[int, str], None]
    ) -> bool:
        """Read the Qwen3 encoder and KEEP it. Caller must hold `self._lock`.

        Gated by `_require_encode_headroom_locked` — the same `encode_standalone`
        phase `prompt_cache.build` is gated by, so a user-pressed load of the
        16 GB encoder is refused with the same numbers and the same advice
        instead of letting the kernel pick a victim.

        **No model-manager lease is taken**, and unlike `prompt_cache.build` that
        is a limitation rather than a clean consequence: `_model_key` names a
        whole pipeline-plus-encoder bundle and `_unload_key` refuses when
        `self._pipe is None`, so leasing a pipeline key for an encoder-only
        residency would register an entry the manager could never evict. Memory
        SAFETY is unaffected — the guard above reads the actual free host memory,
        which a resident encoder has already reduced — but `max_loaded_models`
        under-counts an encoder loaded this way until the user unloads it or the
        next run replaces it. See `inpaint/MODULE_README.md`.
        """
        self._require_encode_headroom_locked(normalized)
        report(LOAD_STEP_PREPARE, "Подготовка загрузки текстового энкодера")
        encoder, encoder_key, _device = self._ensure_text_encoder_locked(
            normalized, report, what="загрузка текстового энкодера"
        )
        self._text_encoder = encoder
        self._text_encoder_key = encoder_key
        report(LOAD_STEP_ENCODER_DONE, "Текстовый энкодер загружен")
        return True

    def _load_pipeline_action_locked(
        self,
        normalized: dict[str, Any],
        model_key: str,
        report: Callable[[int, str], None],
        lease: Any,
    ) -> bool:
        """Build and warm up the transformer + VAE. Caller must hold `self._lock`.

        The lease boundary is the same one `inpaint_image_bytes` documents and
        for the same reason: the load scope ends at `_ensure_pipeline_locked`,
        because everything after it runs with the pipeline already resident, and
        reporting a failed warm-up as a failed LOAD would clear the manager's
        `resident` flag and drop its unload callback while 18 GB still occupy
        VRAM.

        # Raises
        `ValueError` when the configured encoder cannot be used with this
        transformer (`require_encoder_transformer_compatible`), plus whatever
        the memory guard and the pipeline build raise.
        """
        try:
            # Same guard as the generation path, and for the same reason: this
            # button reads the transformer, so an encoder that cannot ever be
            # used with it must be refused BEFORE the 18 GB, not after.
            require_encoder_transformer_compatible(normalized)
            self._require_pipeline_headroom_locked(normalized, model_key)
            report(LOAD_STEP_PREPARE, "Подготовка загрузки FLUX.2 klein")
            pipe = self._ensure_pipeline_locked(
                normalized,
                model_key,
                report,
                # No image is produced here; the smallest valid region is what the
                # log line names, exactly as in `_require_encode_headroom_locked`.
                region_hw=(MIN_REGION_SIDE, MIN_REGION_SIDE),
            )
        except Exception:
            if lease is not None and lease.needs_load:
                lease.mark_load_failed()
            raise
        if lease is not None and lease.needs_load:
            lease.mark_loaded(unload_callback=lambda: self._unload_key(model_key))
        # The guarded form even though this action is only offered while nothing
        # is loaded: it makes the once-per-placement rule hold on every route into
        # the pipeline rather than on the generation route alone.
        self._warmup_pipeline_if_needed_locked(pipe, normalized, report)
        return True

    def _restore_transformer_action_locked(
        self, normalized: dict[str, Any], report: Callable[[int, str], None]
    ) -> bool:
        """Move a parked transformer back onto the device. Caller holds `self._lock`.

        Gated by `_require_restore_headroom_locked` first, because this is a full
        9B host->device copy. A failure here leaves the transformer on the host
        while `_active_key` still claims a placed pipeline, so the cached
        pipeline is invalidated exactly as `_decode_locked` does after a failed
        restore — the next request then rebuilds instead of cache-hitting onto a
        device mismatch. The original error is re-raised.

        A SUCCESSFUL restore is still a weight move, so the pipeline is left
        marked cold: the copy `nn.Module.to` queues here is exactly the one the
        warm-up exists to retire and verify.
        """
        self._require_restore_headroom_locked(normalized)
        report(LOAD_STEP_PLACEMENT, "Возврат трансформера на устройство")
        try:
            pipeline._restore_transformer_to_device(self._pipe, self._device)
        except Exception as exc:
            self._invalidate_pipeline_locked(
                "трансформер не удалось вернуть на устройство по запросу пользователя", exc
            )
            raise
        self._pipeline_warmed = False
        return True

    def _require_pipeline_headroom_locked(self, normalized: dict[str, Any], model_key: str) -> None:
        """Gate a user-pressed pipeline load on the free memory.

        Caller must hold `self._lock`. This is the SAME gate a generation runs
        (`_require_memory_headroom` over `forecast_memory`) with the same
        reserves and the same actionable refusal — a button that loads 18 GB of
        transformer must not be a way around the guard that exists because the
        kernel's OOM killer once closed this user's editor. Only `denoise` and
        `decode` are listed: no prompt is encoded here, so charging the encode
        phase would refuse a load that fits.

        The region is the smallest valid one, exactly as in
        `_require_encode_headroom_locked`: the phases are dominated by the
        weights, and no latent and no pixel is produced by a load.
        """
        pipeline_resident = self._pipe is not None and self._active_key == model_key
        _require_memory_headroom(
            normalized,
            MIN_REGION_SIDE,
            MIN_REGION_SIDE,
            hardware._resolve_selected_backend_device("cuda"),
            phases=("denoise", "decode"),
            pipeline_resident=pipeline_resident,
            # The encoder is not read by a pipeline load, so nothing of it is
            # discounted: what it holds is already missing from the free-memory
            # figures this compares against.
            encoder_resident=False,
        )

    def _require_restore_headroom_locked(self, normalized: dict[str, Any]) -> None:
        """Gate moving a parked transformer back onto the device.

        Caller must hold `self._lock`. A 9B host->device copy is exactly the
        allocation the guard exists for, so it runs the `denoise` phase of the
        same forecast. `pipeline_resident=False` deliberately: the transformer is
        on the HOST right now, so its device bytes are not held — the VAE's ~0.2
        GB is therefore charged twice, and erring toward a refusal is the correct
        direction for a guard whose failure mode is an out-of-memory kill.
        """
        _require_memory_headroom(
            normalized,
            MIN_REGION_SIDE,
            MIN_REGION_SIDE,
            hardware._resolve_selected_backend_device("cuda"),
            phases=("denoise",),
            pipeline_resident=False,
            encoder_resident=(
                self._text_encoder is not None
                and self._text_encoder_key == self._encoder_key(normalized, "cpu")
            ),
        )

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
            "dtype": str(normalized["dtype"]),
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

        The placement is deliberately absent — the same encoder computes the same
        embedding whether it ran on the host or the accelerator, and including it
        would evict the cache on a profile change for nothing.
        """
        return (
            normalized["text_encoder_path"],
            text,
            int(normalized["max_sequence_length"]),
            normalized["dtype"],
            bool(normalized["text_encoder_fp8"]),
        )

    def _prompts_to_encode(self, normalized: dict[str, Any]) -> list[str]:
        """Prompt texts the phase still has to run; empty when the cache covers the run."""
        wanted = [normalized["prompt"]]
        if float(normalized["guidance_scale"]) > 1.0:
            # Classifier-free guidance needs the empty prompt too; encoding it now
            # is what keeps the pipeline from reaching for an encoder that is gone.
            wanted.append("")
        return [
            text
            for text in wanted
            if self._prompt_cache_key(normalized, text) not in self._prompt_cache
        ]

    def _require_headroom_locked(
        self, normalized: dict[str, Any], region_width: int, region_height: int, model_key: str
    ) -> None:
        """Gate the request on the free memory, before a single byte is read.

        Caller must hold `self._lock`. Only the phases that will actually load
        something are checked: a cached prompt skips `encode`, a resident
        pipeline skips `denoise`/`decode`. They are listed in the order the run
        performs them — pipeline first, prompt second — so the refusal message
        names the phase the user would have hit first.

        What the service ALREADY holds is reported too, and discounted: a request
        that only needs a new prompt still runs its `encode` phase, but that
        phase's forecast includes the placed pipeline's VRAM and the kept
        encoder's RAM — both of which are already allocated and therefore already
        missing from the free-memory figures the guard compares against. Charging
        for them twice refused a run whose memory was, literally, already in
        place; that was measured on this project's reference host the first time
        a second prompt was sent to a resident pipeline.
        """
        phases: list[str] = []
        pipeline_resident = self._pipe is not None and self._active_key == model_key
        if not pipeline_resident:
            phases.extend(("denoise", "decode"))
        if self._prompts_to_encode(normalized):
            phases.append("encode")
        encoder_resident = (
            self._text_encoder is not None
            and self._text_encoder_key == self._encoder_key(normalized, "cpu")
        )
        _require_memory_headroom(
            normalized,
            region_width,
            region_height,
            hardware._resolve_selected_backend_device("cuda"),
            phases=tuple(phases),
            pipeline_resident=pipeline_resident,
            encoder_resident=encoder_resident,
        )

    def _prompt_embeds_locked(
        self, normalized: dict[str, Any], report: Callable[[int, str], None]
    ) -> dict[str, Any]:
        """Phase 2: return `{"prompt", "negative"}` embeddings, host-resident.

        Caller must hold `self._lock`, and the pipeline must already be built and
        placed — the encoder is read into the host memory the transformer has
        just vacated. On a full cache hit the encoder is not touched at all: a
        mask edit, a new seed or a repeated prompt goes straight to the denoise.
        On a miss the encoder is loaded, used once and (when
        `unload_text_encoder_after_encode` is on) released again; the freed
        memory is measured, not assumed.

        `negative` is `None` unless classifier-free guidance is active.
        """
        missing = self._prompts_to_encode(normalized)
        if missing:
            self._encode_prompts_locked(normalized, missing, report)
        else:
            report(LOAD_STEP_ENCODER_DONE, "Промпт взят из кэша")

        embeds: dict[str, Any] = {"prompt": None, "negative": None}
        embeds["prompt"] = self._cached_embeds(normalized, normalized["prompt"])
        if float(normalized["guidance_scale"]) > 1.0:
            embeds["negative"] = self._cached_embeds(normalized, "")
        return embeds

    def _cached_embeds(self, normalized: dict[str, Any], text: str) -> Any:
        """Fetch one cached embedding and mark it most-recently used."""
        key = self._prompt_cache_key(normalized, text)
        value = self._prompt_cache[key]
        self._prompt_cache.move_to_end(key)
        return value

    def _encode_prompts_locked(
        self, normalized: dict[str, Any], texts: list[str], report: Callable[[int, str], None]
    ) -> None:
        """Load the encoder, encode `texts` into the cache, release the encoder.

        Caller must hold `self._lock`. **The encoder always encodes in HOST
        memory, in every placement.** It used to go on the accelerator under
        `full_gpu`, which was safe only while it ran BEFORE the transformer was
        loaded; now that the transformer is already resident on the card when
        this runs, the two would have to fit there together — 18.3 GB + 16.4 GB
        on a 34.2 GB card, i.e. they do not. The host is what the new order
        frees, so the host is where this phase belongs.

        This is also THE place a missing encoder becomes fatal: every path that
        reads the encoder goes through here, so the check cannot be bypassed by a
        caller that forgot it (the two public entry points check earlier only to
        avoid loading a pipeline they would then throw away).
        """
        encoder, encoder_key, device = self._ensure_text_encoder_locked(
            normalized, report, what="кодирование промпта"
        )

        from diffusers import Flux2KleinInpaintPipeline

        roots = component_search_roots(normalized)
        tokenizer_dir = _require_component_dir(
            roots, _TOKENIZER_SUBDIR, _TOKENIZER_MARKERS, "токенизатор Qwen"
        )
        tokenizer = self._ensure_tokenizer_locked(tokenizer_dir)

        report(LOAD_STEP_ENCODE, "Кодирование промпта")
        for text in texts:
            embeds = _encode_prompt_phase(
                Flux2KleinInpaintPipeline,
                encoder,
                tokenizer,
                text,
                int(normalized["max_sequence_length"]),
                device,
            )
            self._store_embeds(self._prompt_cache_key(normalized, text), embeds)

        if normalized["unload_text_encoder_after_encode"]:
            report(LOAD_STEP_ENCODER_DONE, "Выгрузка текстового энкодера")
            before = hardware.memory_snapshot(str(device))
            self._text_encoder = None
            self._text_encoder_key = None
            del encoder
            hardware._clear_torch_cache()
            after = hardware.memory_snapshot(str(device))
            # Measured, not assumed: the release has to be visible in the host
            # figures before the denoise starts, or it did not happen.
            log.info(
                "FLUX.2 klein: текстовый энкодер выгружен после кодирования — освободилось "
                "%.2f ГиБ RAM и %.2f ГиБ VRAM.",
                (after["ram_free"] - before["ram_free"]) / (1024**3),
                (after["vram_free"] - before["vram_free"]) / (1024**3),
            )
        else:
            report(LOAD_STEP_ENCODER_DONE, "Текстовый энкодер оставлен в памяти")
            self._text_encoder = encoder
            self._text_encoder_key = encoder_key

    def _ensure_text_encoder_locked(
        self, normalized: dict[str, Any], report: Callable[[int, str], None], *, what: str
    ) -> tuple[Any, tuple[Any, ...], Any]:
        """Return `(encoder, encoder key, host device)`, reading it from disk if needed.

        Caller must hold `self._lock`. THE one place the Qwen3 encoder is
        constructed: `_encode_prompts_locked` and the user-pressed
        `component_action("text_encoder", "load")` both go through it, so there
        is a single loader path and a single place the fp8 quantization happens.

        It does NOT publish the encoder into `self._text_encoder` — who keeps it
        is the caller's decision (`unload_text_encoder_after_encode` for the
        encode path, unconditionally for the explicit load), and an encoder that
        was already resident under the same key is returned untouched.

        The device is always the HOST: the encoder never goes on the accelerator,
        because by the time it is read the 18 GB transformer is already there
        (see `_encode_prompts_locked` and the load-order contract). `what` names
        the operation in the refusal raised when no encoder is installed.

        # Raises
        `FileNotFoundError` / `ValueError` from `require_text_encoder` when no
        encoder is configured on this machine, and whatever the loader raises.
        """
        require_text_encoder(normalized, what=what)

        import torch

        dtype = torch.bfloat16 if normalized["dtype"] == "bfloat16" else torch.float16
        device = torch.device("cpu")
        encoder_key = self._encoder_key(normalized, str(device))

        report(LOAD_STEP_TEXT_ENCODER, "Загрузка текстового энкодера")
        if self._text_encoder is not None and self._text_encoder_key == encoder_key:
            return self._text_encoder, encoder_key, device

        from transformers import Qwen3ForCausalLM

        self._release_text_encoder_locked()
        # No `device_map`: it exists to load straight into VRAM, and this phase
        # deliberately never touches the accelerator. `patched_module_to` still
        # wraps the load because `low_cpu_mem_usage` moves tensors with
        # `nn.Module.to` even between host allocations.
        with pipeline.patched_module_to():
            encoder = _load_text_encoder(
                Qwen3ForCausalLM,
                normalized["text_encoder_path"],
                dtype=dtype,
                device_map=None,
                low_cpu_mem_usage=normalized["low_cpu_mem_usage"],
            )
        if normalized["text_encoder_fp8"]:
            pipeline._quantize_text_encoder_fp8(encoder)
        return encoder, encoder_key, device

    def _encoder_key(self, normalized: dict[str, Any], device: str) -> tuple[Any, ...]:
        """Identity of a resident text encoder: path, dtype, fp8 and where it sits."""
        return (
            normalized["text_encoder_path"],
            normalized["dtype"],
            bool(normalized["text_encoder_fp8"]),
            device,
        )

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

    def _release_text_encoder_locked(self) -> None:
        """Drop a resident text encoder, if any. Caller must hold `self._lock`."""
        if self._text_encoder is None:
            return
        self._text_encoder = None
        self._text_encoder_key = None
        hardware._clear_torch_cache()

    # ---- pipeline ----
    def _ensure_pipeline_locked(
        self,
        normalized: dict[str, Any],
        model_key: str,
        report: Callable[[int, str], None],
        *,
        region_hw: tuple[int, int],
    ) -> Any:
        """Phase 1: return the cached pipeline for `model_key`, building it if needed.

        Caller must hold `self._lock`. This runs BEFORE the text encoder is read,
        so at this point the host holds nothing but this pipeline's own weights on
        their way to the accelerator. **The pipeline is built WITHOUT a text
        encoder** (`text_encoder=None`): the prompt is embedded in the next phase,
        the denoise never touches the encoder, and holding 8B of Qwen3 next to the
        9B transformer is precisely the residency this design removes.
        `Flux2KleinInpaintPipeline` tolerates the `None` — `pipe.components` only
        validates the KEY set, `DiffusionPipeline.device` and `_execution_device`
        skip non-modules, and `encode_prompt` is never reached because
        `prompt_embeds` is supplied.

        A pipeline built for another key is dropped and reported to the model
        manager first. Placement follows `normalized["placement"]` and routes
        every host->device weight move through `rocm_mmap_transfer` (a no-op off
        ROCm). `region_hw` is the `(height, width)` this pipeline is built for,
        kept for the log line that names what the residency is being spent on.

        # Raises
        `FileNotFoundError` when a component cannot be found, `ValueError` for an
        unsupported checkpoint layout (fp8_scaled), `RuntimeError` when the
        selected placement needs a GPU and none is available.
        """
        if self._pipe is not None and self._active_key == model_key:
            _apply_vae_memory_options(self._pipe, normalized)
            return self._pipe

        prev = self._active_key
        self._pipe = None
        self._active_key = None
        # A pipeline that is about to be built has not been warmed up, and the one
        # being dropped takes its warm-up with it.
        self._pipeline_warmed = False
        hardware._clear_torch_cache()
        if prev is not None:
            self._model_manager.mark_unloaded(prev)

        import torch

        from diffusers import (
            AutoencoderKLFlux2,
            FlowMatchEulerDiscreteScheduler,
            Flux2KleinInpaintPipeline,
            Flux2Transformer2DModel,
        )

        dtype = torch.bfloat16 if normalized["dtype"] == "bfloat16" else torch.float16
        device = torch.device(hardware._resolve_selected_backend_device("cuda"))
        placement = normalized["placement"]
        if device.type == "cpu":
            if placement in _GPU_ONLY_PLACEMENTS:
                raise RuntimeError(
                    f"Режим размещения «{placement}» требует GPU, но доступен только CPU. "
                    "Выберите устройство в настройках или режим «full_gpu»."
                )
            log.warning(
                "FLUX.2 klein: GPU не найден, модель будет работать на CPU — это очень медленно."
            )

        region_height, region_width = region_hw
        log.info(
            "FLUX.2 klein: сборка пайплайна (трансформер + VAE, без текстового энкодера) для "
            "области %dx%d в режиме «%s» на %s.",
            region_width,
            region_height,
            normalized["placement"],
            device,
        )

        roots = component_search_roots(normalized)
        tokenizer_dir = _require_component_dir(
            roots, _TOKENIZER_SUBDIR, _TOKENIZER_MARKERS, "токенизатор Qwen"
        )
        scheduler_dir = _require_component_dir(
            roots, _SCHEDULER_SUBDIR, (_SCHEDULER_MARKER,), "планировщик (scheduler)"
        )

        # `device_map` is accelerate's direct-to-VRAM path: it never calls
        # `nn.Module.to`, so on ROCm it cannot be staged through
        # `rocm_mmap_transfer`. Measured 2026-09-02 on this project's ROCm host
        # (AMD Radeon AI PRO R9700 / gfx1201, torch 2.12.0+rocm7.2,
        # diffusers 0.39.0, accelerate 1.12.0) with the klein VAE (168 MB, 250
        # BF16 tensors, 42 of them >= 1 MiB), page cache dropped before each run,
        # two runs per variant: `device_map` 1.35 s / 1.12 s, CPU load + staged
        # `.to()` 1.38 s / 1.13 s, CPU load + UNSTAGED `.to()` 1.46 s / 1.27 s.
        # All 42 large tensors report `tensor_needs_staging() == True`, yet the
        # unstaged move itself costs 0.10 s, not the 42-84 s the amdkfd stall
        # would imply — the pathology does not reproduce through this loader on
        # this driver. Skipping the staging seam therefore costs nothing
        # measurable here; re-measure before assuming it still holds on another
        # ROCm host. `device_map` is also incompatible with accelerate's own
        # offload hooks, so the offload placements only get the
        # `low_cpu_mem_usage` kwarg itself.
        device_map: dict[str, str] | None = None
        if normalized["low_cpu_mem_usage"] and placement in ("full_gpu", "encoder_cpu"):
            device_map = {"": str(device)}
        elif normalized["low_cpu_mem_usage"]:
            log.info(
                "FLUX.2 klein: режим «%s» управляет размещением через accelerate, поэтому веса "
                "грузятся в обычную память, а low_cpu_mem_usage применяется только к самой загрузке.",
                placement,
            )

        report(LOAD_STEP_TRANSFORMER, "Загрузка трансформера")
        transformer = _load_transformer(
            Flux2Transformer2DModel,
            normalized["transformer_path"],
            dtype=dtype,
            device_map=device_map,
            low_cpu_mem_usage=normalized["low_cpu_mem_usage"],
        )

        # The tokenizer is a pipeline component even though the denoise never
        # uses it: `prompt_embeds` are already computed. It costs a few MB, and it
        # is shared with the prompt-encode path rather than read twice.
        report(LOAD_STEP_TOKENIZER, "Загрузка токенизатора")
        tokenizer = self._ensure_tokenizer_locked(tokenizer_dir)

        report(LOAD_STEP_VAE, "Загрузка VAE")
        vae = _load_vae(
            AutoencoderKLFlux2,
            normalized["vae_path"],
            dtype=dtype,
            device_map=device_map,
            low_cpu_mem_usage=normalized["low_cpu_mem_usage"],
        )

        report(LOAD_STEP_SCHEDULER, "Загрузка планировщика")
        scheduler = FlowMatchEulerDiscreteScheduler.from_pretrained(str(scheduler_dir))

        # `is_distilled` is deliberately left at False: the pipeline uses it for
        # nothing but `do_classifier_free_guidance`
        # (`guidance_scale > 1 and not is_distilled`), and the transformer always
        # receives `guidance=None`. With the default `guidance_scale` of 1.0 the
        # run is therefore identical to a distilled configuration, while a user
        # who raises the scale gets real classifier-free guidance instead of a
        # silently ignored slider. Nothing about the user's checkpoint is assumed.
        pipe = Flux2KleinInpaintPipeline(
            scheduler=scheduler,
            vae=vae,
            # The prompt phase runs AFTER this build and hands its embeddings to
            # `__call__`; see this method's docstring for why the `None` is safe.
            text_encoder=None,
            tokenizer=tokenizer,
            transformer=transformer,
        )
        pipe.set_progress_bar_config(disable=True)
        _apply_vae_memory_options(pipe, normalized)

        report(LOAD_STEP_PLACEMENT, "Размещение модели")
        _apply_placement(pipe, placement, device)

        self._pipe = pipe
        self._device = device
        self._active_key = model_key
        return pipe

    # ---- warm-up: proof that the weights really left the host ----
    def _warmup_pipeline_if_needed_locked(
        self, pipe: Any, normalized: dict[str, Any], report: Callable[[int, str], None]
    ) -> bool:
        """Warm up the pipeline unless its current placement is already warm.

        Caller must hold `self._lock`. Returns whether a warm-up actually ran, so
        `False` means either "already warm" or "this placement owes none".

        **The warm-up is owed once per PLACEMENT of the weights, not once per
        request.** Everything `_warmup_pipeline_locked` establishes is a fact
        about a placement — no parameter left on `meta` or on the host, the queued
        host->device copies retired, the host pages released before the 16 GB
        encoder is read — and none of it can stop being true while no weight
        moves. Running it per request cost a full parameter walk over the 9B
        transformer, a real VAE decode and an allocator flush on every hot run,
        and announced «Прогрев модели» to a user whose model had been resident
        for an hour.

        `self._pipeline_warmed` therefore means "the placement currently in
        `self._pipe` has been warmed since the weights last moved". It is cleared
        by every path that moves them, and this list is exhaustive by design —
        a path that moved weights without clearing it would let the next run skip
        a warm-up it genuinely needed and fail inside the VAE with the device
        mismatch `_require_components_materialized` exists to pre-empt:

        - `_ensure_pipeline_locked`, on the branch that drops and rebuilds;
        - `_unload_pipeline_locked` (so also `unload()` and the `_unload_key`
          eviction callback) and `_invalidate_pipeline_locked`;
        - the transformer park, both through the `to_ram` action and through
          `_decode_locked`'s `unload_transformer_before_vae` path;
        - the restore back onto the device, both through the `to_gpu` action and
          through `_decode_locked`'s `finally` — a successful restore leaves the
          pipeline COLD, because its copy is queued exactly like a placement's.

        The consequence for `unload_transformer_before_vae` (the default in every
        placement but `full_gpu`) is deliberate: that setting parks the
        transformer on every run, so every run owes a warm-up, exactly as before.
        `full_gpu` is where a hot pipeline stays hot.
        """
        if self._pipeline_warmed:
            log.debug(
                "FLUX.2 klein: прогрев не требуется — веса не перемещались с прошлого прогрева."
            )
            return False
        return self._warmup_pipeline_locked(pipe, normalized, report)

    def _warmup_pipeline_locked(
        self, pipe: Any, normalized: dict[str, Any], report: Callable[[int, str], None]
    ) -> bool:
        """Force the placed weights to materialize, and prove that they did.

        Caller must hold `self._lock`. Returns whether a warm-up actually ran.
        Unconditional: the "already warm" question belongs to
        `_warmup_pipeline_if_needed_locked`, and the user-pressed `warmup` action
        deliberately comes straight here. On every normal return it marks the
        pipeline warm — including the two skips below, which mean "nothing is
        owed", not "not done yet". A raise leaves it cold.

        This is the hinge of the new load order. The text encoder is read
        immediately afterwards, and the only reason it fits is that the
        transformer's 18 GB have left host memory by then. "Placed" is not the
        same as "materialized": `nn.Module.to` returns as soon as the copies are
        QUEUED, accelerate's `device_map` path can leave a `meta` parameter
        behind, and the host-side source pages are released only once the copy
        has retired. So the warm-up does three things, in order:

        1. checks that no parameter of the transformer or the VAE is still on
           `meta` or on the host (`_require_components_materialized`) — a named
           error here beats a device mismatch several frames inside the VAE;
        2. runs ONE tiny VAE decode (`WARMUP_LATENT_CELLS` latent cells, i.e. a
           64x64 image), which is a real forward: it retires the weight copies,
           initializes the caching allocator and, on ROCm, compiles the MIOpen
           convolution kernels the real decode will reuse;
        3. releases the transient blocks and logs the measured host/device
           figures, so the claim "the host is free now" is a measurement.

        It is a `phase:"load"` step (`LOAD_STEP_WARMUP`) and deliberately NOT a
        generation step: `_generate_locked` owns the `phase:"generate"` counter,
        and a warm-up counted there would make the progress bar report a step the
        user did not ask for.

        Skipped under the two accelerate offload placements: there the weights are
        SUPPOSED to sit in host memory between forwards, so there is nothing to
        materialize and a warm-up would drag all 9B onto the card and back for
        nothing. Also skipped when the pipeline exposes no VAE (a test double).
        """
        placement = normalized["placement"]
        if placement in ("model_cpu_offload", "sequential_cpu_offload"):
            log.debug(
                "FLUX.2 klein: прогрев пропущен — в режиме «%s» веса намеренно живут в "
                "оперативной памяти между проходами.",
                placement,
            )
            self._pipeline_warmed = True
            return False
        vae = getattr(pipe, "vae", None)
        if vae is None or not hasattr(vae, "decode"):
            self._pipeline_warmed = True
            return False

        _require_components_materialized(pipe, self._device)
        report(LOAD_STEP_WARMUP, "Прогрев модели")
        before = hardware.memory_snapshot(str(self._device))
        started_at = time.perf_counter()
        _warmup_vae_decode(pipe, self._device)
        hardware._clear_torch_cache()
        after = hardware.memory_snapshot(str(self._device))
        log.info(
            "FLUX.2 klein: прогрев выполнен за %.2f с — веса трансформера и VAE материализованы на "
            "%s. Свободно: %.2f ГиБ RAM (было %.2f), %.2f ГиБ VRAM (было %.2f). Текстовый энкодер "
            "загружается следующим, уже в освободившуюся оперативную память.",
            time.perf_counter() - started_at,
            self._device,
            after["ram_free"] / (1024**3),
            before["ram_free"] / (1024**3),
            after["vram_free"] / (1024**3),
            before["vram_free"] / (1024**3),
        )
        self._pipeline_warmed = True
        return True

    def _unload_key(self, model_key: str) -> bool:
        """Eviction callback: drop the pipeline only if it still holds `model_key`."""
        with self._lock:
            if self._pipe is None or self._active_key != model_key:
                return False
            return self.unload()

    # ---- generation ----
    def _generate_locked(
        self,
        pipe: Any,
        region_rgb: np.ndarray,
        mask_u8: np.ndarray,
        normalized: dict[str, Any],
        embeds: dict[str, Any],
        progress_callback: ProgressCb | None,
    ) -> tuple[np.ndarray, dict[str, bool], bool]:
        """Run the pipeline once and composite the result over the region.

        `embeds` is the prompt phase's output: `{"prompt", "negative"}`, host-resident.
        The pipeline is ALWAYS called with `prompt=None` and those embeddings —
        it has no text encoder to fall back on, in any placement.

        Caller must hold `self._lock`. The LATENT mask handed to the pipeline is
        dilated by `mask_dilate_px` so the model has room to blend; the COMPOSITE
        uses the original mask feathered inwards by `mask_feather_px`, which is
        what keeps every pixel outside the mask byte-identical.

        Under `whole_region` this method needs no special case: normalization has
        already set `mask_dilate_px` to 0 (a solid mask has nothing to grow into)
        and `color_match` to `False` (there is no unchanged ring to match
        against) — see `_whole_region_overrides`. The feather still applies, and
        on a solid mask it ramps inwards from the region's own border, which is
        exactly the soft join to the rest of the page that mode wants.

        Denoising and decoding are two separate steps: the pipeline is asked for
        latents, a CPU copy of them is kept, and the VAE decode runs afterwards —
        optionally with the transformer parked off the GPU, and with an OOM
        recovery path that retries the decode instead of the whole run.

        Returns `(rgb, applied, oom_recovered)` where `applied` names the memory
        settings actually in force at the end and `oom_recovered` says whether a
        retry was needed.
        """
        import numpy as np
        import torch
        from PIL import Image

        height, width = region_rgb.shape[:2]
        latent_mask = _dilate_mask(mask_u8, normalized["mask_dilate_px"])

        seed = normalized["seed"]
        generator = torch.Generator("cpu")
        generator = generator.manual_seed(
            int(seed) if seed is not None else int.from_bytes(os.urandom(4), "little")
        )

        requested_steps = int(normalized["steps"])
        total_steps = effective_steps(requested_steps, float(normalized["strength"]))
        cb = progress_callback

        def _on_step(_pipe: Any, step: int, _t: Any, kwargs: dict[str, Any]) -> dict[str, Any]:
            if cb is not None:
                try:
                    cb("generate", int(step) + 1, total_steps, "Генерация")
                except Exception:  # noqa: BLE001 - a dead peer must not kill the run
                    pass
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
            "guidance_scale": float(normalized["guidance_scale"]),
            "max_sequence_length": int(normalized["max_sequence_length"]),
            "generator": generator,
            # Latents only: the VAE decode is a separate step so the transformer
            # can leave the GPU first, and the pipeline's own postprocess is
            # useless to us anyway — the composite, color match and feather are
            # ours.
            "output_type": "latent",
            "callback_on_step_end": _on_step,
            "callback_on_step_end_tensor_inputs": ["latents"],
        }

        # `self._device` is the placement target `_ensure_pipeline_locked` chose,
        # which is what the run must happen on; the probe is checked against it
        # rather than against a component's own device, because a component that
        # failed to be placed would otherwise define the target as wherever it
        # happens to sit.
        if normalized["placement"] in ("full_gpu", "encoder_cpu"):
            _require_execution_device(pipe, self._device)
        negative = embeds["negative"]
        result = pipe(
            prompt=None,
            prompt_embeds=embeds["prompt"].to(device=self._device),
            negative_prompt_embeds=None if negative is None else negative.to(device=self._device),
            **call_kwargs,
        )

        # Keep the latents in host memory before anything else touches the GPU:
        # at 1 MP they are a few hundred KiB, and holding them is what lets an
        # OOM in the decode be retried without repeating the denoise.
        latents_cpu = result.images.detach().to("cpu")

        decoded, applied, oom_recovered = self._decode_locked(pipe, latents_cpu, normalized)
        generated = np.ascontiguousarray(np.asarray(decoded.convert("RGB"), dtype=np.uint8))
        if generated.shape[:2] != (height, width):
            raise RuntimeError(
                f"Пайплайн вернул область {generated.shape[1]}x{generated.shape[0]} вместо "
                f"{width}x{height}; композит невозможен"
            )

        if normalized["color_match"]:
            generated = _match_color_outside_mask(generated, region_rgb, latent_mask)
        composed = _composite_over_region(
            region_rgb, generated, mask_u8, int(normalized["mask_feather_px"])
        )
        return composed, applied, oom_recovered

    def _decode_locked(
        self, pipe: Any, latents_cpu: Any, normalized: dict[str, Any]
    ) -> tuple[Any, dict[str, bool], bool]:
        """VAE-decode `latents_cpu`, parking the transformer and recovering from OOM.

        Caller must hold `self._lock`. When the transformer was moved off the
        device it is moved back before returning, so the resident pipeline still
        matches its model key and the next request is a plain cache hit — a cache
        hit that is marked COLD, because the park and the restore are two weight
        moves and a warm-up is owed per placement
        (`_warmup_pipeline_if_needed_locked`).

        The move back can itself run out of memory (it is a full 9B host->device
        copy). That failure never reaches the caller: it must not mask a decode
        that already succeeded, and it must not mask a decode that already
        failed for a more informative reason. Instead the cached pipeline is
        invalidated — its transformer is on the host, so the next cache hit would
        skip placement and fail on a device mismatch — and the failure, with the
        decode error still in its `__context__` chain, goes to the log.
        """
        placement = normalized["placement"]
        parked = False

        def park() -> bool:
            """Take the transformer off the device once; `True` if it moved."""
            nonlocal parked
            if parked:
                return False
            moved = _park_transformer_off_device(pipe, placement)
            if moved:
                # The placement this pipeline was warmed for no longer holds; the
                # restore in the `finally` below re-queues the copy rather than
                # re-proving it, so the next request owes a fresh warm-up.
                self._pipeline_warmed = False
            parked = parked or moved
            return moved

        if normalized["unload_transformer_before_vae"]:
            park()
        try:
            return _decode_region_latents(pipe, latents_cpu, normalized, park)
        finally:
            if parked:
                try:
                    pipeline._restore_transformer_to_device(pipe, self._device)
                except Exception as restore_exc:  # noqa: BLE001 - becomes an invalidation
                    self._invalidate_pipeline_locked(
                        "трансформер не удалось вернуть на устройство после декодирования VAE",
                        restore_exc,
                    )

    def _invalidate_pipeline_locked(self, reason: str, cause: BaseException) -> None:
        """Drop the cached pipeline after its state stopped matching its key.

        Caller must hold `self._lock`. `cause` is logged with its full chain (its
        `__context__` still carries whatever failed first, if anything did), and
        the model manager is told the key is gone, so the entry stops counting
        toward `max_loaded_models` and the next request rebuilds instead of
        taking a cache hit on a pipeline whose components are half-placed.
        """
        key = self._active_key
        self._pipe = None
        self._active_key = None
        # The pipeline this warm-up described is gone with it.
        self._pipeline_warmed = False
        hardware._clear_torch_cache()
        if key is not None:
            self._model_manager.mark_unloaded(key)
        log.error(
            "FLUX.2 klein: %s. Кэш пайплайна сброшен (ключ %s), следующий запрос загрузит "
            "модель заново.",
            reason,
            key if key is not None else "—",
            exc_info=cause,
        )
