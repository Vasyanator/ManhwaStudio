"""
File: modules/ai_backend/inpaint/flux2_klein/memory.py

Purpose:
The RAM/VRAM forecast for one FLUX.2 klein run and the pre-load guard built on
top of it. `forecast_memory` turns a normalized request plus the on-disk weight
sizes into a per-phase host/device requirement; `_require_memory_headroom`
refuses a run that would not fit and names the placement preset that would.

The numbers here are COARSE, advisory upper bounds, not measurements: the guard
exists because a host-side shortfall is not an exception but an OOM kill, so it
is better to refuse early with an explanation than to die mid-load.

Main responsibilities:
- `forecast_memory` - per-phase host/device bytes for a normalized request;
- `_require_memory_headroom` - the refusal, with `_preset_advice` naming a
  placement that would fit;
- the advisory constants (activation sizes, decode cost per pixel, host/device
  reserves, `_MEMORY_PRESETS`).

Notes:
`components.text_encoder_resident_bytes` and `hardware.memory_snapshot` are
reached through their modules on purpose: both are replaced by the test suite,
and a `from ... import` here would leave this module holding a stale reference.

The decode phase asks `components.vae_tile_threshold_pixels` whether the VAE's
tiled path can engage for this region at all: `vae_tiling` is a REQUEST, and a
request that cannot be honoured must not be forecast as a saving. The pipeline's
transient host copy follows the same rule through
`_transformer_avoids_the_host`: `low_cpu_mem_usage` is a request too, and
diffusers' `from_single_file` ignores it.

This module asks `components.text_encoder_resident_bytes` for the encoder and
`components._weight_bytes` for the transformer and the VAE, and that split is
deliberate: the encoder is loaded truncated, so what it occupies in RAM is
smaller than what it occupies on disk, while `status.components[*].size_bytes`
must keep showing the disk figure. Do not collapse the two.
"""

from __future__ import annotations

import logging
from pathlib import Path
from typing import Any

from . import components, hardware, streaming
from .components import text_encoder_available
from .params import REGION_SIZE_MULTIPLE

log = logging.getLogger(__name__)

#: Peak activation bytes of ONE text-encoder forward pass at 512 tokens: the
#: hidden states of every kept layer (`output_hidden_states=True` retains all of
#: them, not just the three that are stacked), the stacked
#: `(1, 512, 3 * hidden)` embedding, and the attention working set. The encoder
#: is loaded as `Qwen3Model`, so there is no `[1, 512, vocab]` logits tensor to
#: budget for any more — the value is kept at 512 MiB anyway, as a coarse upper
#: bound in the spirit of the other constants here, not as a measurement.
ENCODE_ACTIVATION_BYTES = 512 * 1024 * 1024

# ---------------------------------------------------------------------------
# Memory forecast constants
# ---------------------------------------------------------------------------
# These are COARSE, advisory upper bounds, not measurements: nothing in this
# repository profiles FLUX.2 klein, and `estimate` exists to keep a user from
# starting a run that obviously cannot fit. Every number below is per unit of
# work and independent of the checkpoint, so a wrong one is off by a constant
# factor and never by an order of magnitude.

#: Peak transformer activation bytes per latent token (residual stream plus the
#: attention working set at bf16, ~16 buffers of a 3072-wide hidden state).
ACTIVATION_BYTES_PER_LATENT_TOKEN = 96 * 1024

#: Peak VAE decode activation bytes per output pixel, untiled: full-resolution
#: feature maps of ~128 channels at 2 bytes, a few buffers deep.
VAE_DECODE_BYTES_PER_PIXEL = 1024

#: The same with tiling/slicing enabled — only one tile is live at a time.
VAE_DECODE_TILED_BYTES_PER_PIXEL = 256

#: Share of the transformer resident on the device under sequential offload
#: (roughly one block plus the layer being prefetched).
SEQUENTIAL_RESIDENT_TRANSFORMER_FRACTION = 0.08

# ---------------------------------------------------------------------------
# Pre-load memory guard
# ---------------------------------------------------------------------------
# The guard exists because a host-side shortfall is NOT an exception: the kernel
# OOM killer picks a victim among everything running, and on this project's
# reference host it has already closed a user's editor with unsaved work while
# the 9B transformer and the 8B encoder were being loaded side by side. A
# `torch.OutOfMemoryError` would have been the good outcome.

#: Host memory the guard refuses to plan into. It covers what the forecast does
#: not model and what a shortfall costs someone else: the interpreter and torch
#: runtime already resident, the transient copy buffers a safetensors read needs,
#: and the margin the kernel wants before it starts killing processes. 2 GiB is
#: roughly one large shard's working set — small enough not to reject a run that
#: genuinely fits, large enough that the OOM killer is not the next event.
HOST_MEMORY_RESERVE_BYTES = 2 * 1024**3

#: Device memory the guard refuses to plan into: allocator fragmentation plus the
#: BLAS/attention workspaces that are not part of the per-token activation
#: constant. Smaller than the host reserve because running out here raises a
#: catchable `OutOfMemoryError` and the decode has its own recovery ladder.
DEVICE_MEMORY_RESERVE_BYTES = 512 * 1024**2

#: The four memory profiles the UI offers, as (label, placement,
#: low_cpu_mem_usage). MUST stay in sync with `MemoryPreset::values` in
#: `src/tabs/cleaning/tools/ai_editor/engines/flux2_klein/settings.rs` — the guard names the ones that fit,
#: so a stale entry here is advice the user cannot follow.
_MEMORY_PRESETS = (
    # The labels are shown to the USER by `_preset_advice`, so they must be the
    # strings the picker shows, not paraphrases of them: advice naming a preset
    # the user cannot find in the UI is advice they cannot follow. Mirror of
    # `cleaning.tools.flux2_klein.preset_*` in `crates/ms-i18n/locales/*.json`.
    ("Максимум скорости", "full_gpu", False),
    ("Сбалансированный", "encoder_cpu", False),
    ("Минимум RAM", "encoder_cpu", True),
    ("Минимум VRAM", "sequential_cpu_offload", True),
)


#: The accelerator target the forecast ASSUMES when it asks
#: `streaming.streaming_load_eligible` whether the single-file transformer will
#: really be streamed. The forecast answers before any device is resolved, so
#: this one precondition — the only one of the four that is about the MACHINE
#: rather than the request — cannot be read from `normalized` and is assumed
#: instead. The assumption is the same one the whole non-offload branch below
#: already makes (`pipeline_device = pipeline_bytes`, `pipeline_host_resident =
#: 0`: the weights end up on the card), so it adds no new optimism. Every other
#: precondition is answered from the request, and each one that cannot be proved
#: yields the EXPENSIVE answer — "streaming will not run", i.e. the whole
#: checkpoint on the host.
_FORECAST_STREAMING_TARGET = "cuda"


def _transformer_avoids_the_host(normalized: dict[str, Any], *, low_cpu: bool) -> bool:
    """Whether the transformer load really keeps the checkpoint out of host memory.

    Only meaningful for the two non-offload placements, whose caller is the one
    branch of `forecast_memory` that may ask; the offload placements keep the
    weights in host memory by design.

    `low_cpu_mem_usage` alone does NOT answer this, and the difference is the
    largest single term in the host forecast:

    - a diffusers FOLDER goes through `from_pretrained` with a whole-model
      `device_map`, i.e. accelerate's shard-by-shard path, which really does
      write straight to the device;
    - a single FILE goes through diffusers' `from_single_file`, which reads the
      ENTIRE checkpoint into host memory first (`load_single_file_checkpoint`)
      whatever `low_cpu_mem_usage` says — unless
      `streaming.load_transformer_streaming` takes it instead, which is exactly
      what `streaming.streaming_load_eligible` decides.

    Under-forecasting here is the unrecoverable direction: a host shortfall is
    the kernel's OOM killer, not a catchable exception. So an unreadable, sharded
    or otherwise unclassifiable checkpoint answers `False` — the expensive
    answer — rather than being credited with a saving it may not get.
    """
    if not low_cpu:
        return False
    source = Path(normalized["transformer_path"])
    if source.is_dir():
        return True
    eligible, _reason = streaming.streaming_load_eligible(
        source,
        low_cpu_mem_usage=low_cpu,
        device_map={"": _FORECAST_STREAMING_TARGET},
    )
    return eligible


# =====================================================================
#  Memory forecast and the pre-load guard
# =====================================================================
def forecast_memory(
    normalized: dict[str, Any], region_width: int, region_height: int
) -> dict[str, Any]:
    """Forecast the device and host memory one run costs, in bytes.

    THE single forecast in this module: `estimate` reports it to the UI and
    `_require_memory_headroom` gates the load on it. Two independent
    calculations would drift, and a guard that disagrees with the number on
    screen is worse than no guard.

    **A run is a sequence of phases, so the answer is their MAXIMUM, not their
    sum.** `encode`, `denoise` and `decode` are forecast separately and reported
    in `breakdown` (`encode_standalone` is a fourth entry that no run performs:
    it is what a prompt-cache BUILD costs, and it is dominated by `encode`, so it
    changes none of the totals), and the arithmetic follows the order the run
    actually uses:
    the transformer and the VAE are loaded and placed FIRST, and the text encoder
    is read afterwards, into host memory, in every placement.

    That order is what the host figures below turn on. Under the two non-offload
    placements the pipeline's host copy exists only while it is on its way to the
    accelerator, and the encoder arrives after it is gone — so the two host peaks
    never coexist and the `denoise` host term is their MAXIMUM, not their sum.
    Under the two offload placements the pipeline stays in host memory for the
    whole run, and that same maximum degenerates into the sum it should be.

    **How big that transient copy is follows the LOADER, not the flag.**
    `low_cpu_mem_usage` is a request, and `from_single_file` ignores it: a
    single-file transformer is materialized in full in host memory unless the
    streaming loader takes it. `_transformer_avoids_the_host` answers that
    question, and answers it EXPENSIVELY whenever it cannot prove the saving.

    The weight terms are read from the files on disk (a bf16/fp16 checkpoint
    stores 2 bytes per parameter, which is also what it occupies once loaded);
    the activation terms use the coarse per-token / per-pixel constants
    documented at the top of this module. The TEXT ENCODER is the one component
    whose forecast is not its file size: it is loaded truncated, so
    `components.text_encoder_resident_bytes` answers for it, and every encoder
    figure this function returns — the `encode` phases, `resident.text_encoder_host`
    and `breakdown.text_encoder` — is a RESIDENT figure. `status` keeps reporting
    the DISK size for the same component, and the two legitimately disagree.

    **The decode's per-pixel constant follows what the VAE will DO, not what the
    request asked for.** `vae_tiling` only flips `use_tiling`; diffusers enters
    the tiled decode only when a side exceeds the VAE's own threshold, so the
    cheap constant is used only when `components.vae_tile_threshold_pixels` says
    this region reaches it. `vae_slicing` never enters the predicate: slicing
    splits a BATCH and this service decodes one image.

    **With no text encoder installed both encode phases are zero**, because
    neither can run there: a prompt that is not cached is refused before the load
    and a cached one skips the phase. The forecast is lower on such a machine,
    and it is lower in the guard and on screen at once — there is still exactly
    one calculation.

    Returns `{"vram_bytes", "ram_bytes", "phases", "resident", "breakdown"}`.
    `phases` maps each phase name to its own `{"vram_bytes", "ram_bytes"}` — that
    is what the guard checks one at a time. `resident` names the two costs a run
    can arrive with ALREADY PAID, so the guard can discount them without doing
    its own arithmetic; see `_require_memory_headroom`.

    # Raises
    `ValueError` for a placement outside `VALID_PLACEMENTS`.
    """
    transformer_bytes = components._weight_bytes(normalized["transformer_path"])
    # No encoder on this machine means no encode phase can ever run: a prompt
    # that is not already cached is refused before a single byte is read
    # (`require_text_encoder`), and a cached one skips the phase entirely. Its
    # cost is therefore ZERO rather than "the size of a file that is not there" —
    # which is the same number `_weight_bytes` would return for a path that
    # exists but holds no weights, and those two must not be confused.
    encoder_installed = text_encoder_available(normalized)
    # RESIDENT bytes, not the disk size: the encoder is loaded as a truncated
    # `Qwen3Model`, so its `lm_head` and every decoder layer at or above
    # `ENCODER_KEEP_LAYERS` never exist in memory (~20% of the shipped 4B
    # encoder's tensors). `_weight_bytes` deliberately still answers the DISK
    # question for `status.components[*].size_bytes`; the two figures differ for
    # this one component and must not be unified.
    text_encoder_bytes = (
        components.text_encoder_resident_bytes(normalized["text_encoder_path"])
        if encoder_installed
        else 0
    )
    vae_bytes = components._weight_bytes(normalized["vae_path"])

    latent_tokens = (int(region_width) // REGION_SIZE_MULTIPLE) * (
        int(region_height) // REGION_SIZE_MULTIPLE
    )
    # Tiling is credited only when it can ACTUALLY engage for this region.
    # `enable_tiling()` flips a flag; the tiled decode path is entered only when a
    # side exceeds the VAE's own threshold (`components.vae_tile_threshold_pixels`,
    # 1024 px on the shipped klein VAE), so with `vae_tiling` defaulting to `True`
    # the old flag-only predicate under-forecast the decode of EVERY region up to
    # that side by a factor of four — in a guard whose failure mode is the OOM
    # killer. `vae_slicing` is not part of the predicate at all: `decode` slices
    # only when `z.shape[0] > 1` and this service always decodes a batch of one.
    tiling_engages = bool(normalized["vae_tiling"]) and max(
        int(region_width), int(region_height)
    ) > components.vae_tile_threshold_pixels(normalized["vae_path"])
    vae_per_pixel = (
        VAE_DECODE_TILED_BYTES_PER_PIXEL if tiling_engages else VAE_DECODE_BYTES_PER_PIXEL
    )
    denoise_activations = latent_tokens * ACTIVATION_BYTES_PER_LATENT_TOKEN
    decode_activations = int(region_width) * int(region_height) * vae_per_pixel

    placement = normalized["placement"]
    low_cpu = bool(normalized["low_cpu_mem_usage"])
    pipeline_bytes = transformer_bytes + vae_bytes

    # fp8 halves the weights that STAY resident. It cannot lower the encode peak:
    # the bf16 weights have to exist before they can be quantized.
    resident_encoder = 0
    if not normalized["unload_text_encoder_after_encode"]:
        resident_encoder = (
            text_encoder_bytes // 2 if normalized["text_encoder_fp8"] else text_encoder_bytes
        )

    if placement in ("full_gpu", "encoder_cpu"):
        # The pipeline ends up on the accelerator, so its host copy is transient:
        # it exists between the safetensors read and the placement move.
        # `low_cpu_mem_usage` removes it only where the loader honours it — the
        # VAE always does (accelerate's `device_map` path), the transformer only
        # as a diffusers folder or through the streaming loader. Asking the flag
        # alone used to report ZERO host bytes for a single-file transformer that
        # `from_single_file` materializes in full, i.e. it under-forecast ~17 GiB
        # on the very path most likely to get the user's editor OOM-killed.
        pipeline_device = pipeline_bytes
        pipeline_host_resident = 0
        if not low_cpu:
            pipeline_host_transient = pipeline_bytes
        elif _transformer_avoids_the_host(normalized, low_cpu=low_cpu):
            pipeline_host_transient = 0
        else:
            pipeline_host_transient = transformer_bytes
        decode_weights_parked = vae_bytes
        # Parking the transformer before the VAE decode copies all 9B of it back
        # into anonymous host memory. That peak is what the OOM killer sees, so
        # it belongs in the RAM forecast of the decode phase.
        parked_ram = transformer_bytes if normalized["unload_transformer_before_vae"] else 0
    elif placement == "model_cpu_offload":
        # Accelerate keeps exactly one component on the device at a time and the
        # rest in host memory, for the whole run.
        pipeline_device = max(transformer_bytes, vae_bytes)
        pipeline_host_resident = pipeline_bytes
        pipeline_host_transient = pipeline_bytes
        decode_weights_parked = vae_bytes
        parked_ram = 0
    elif placement == "sequential_cpu_offload":
        pipeline_device = int(transformer_bytes * SEQUENTIAL_RESIDENT_TRANSFORMER_FRACTION)
        pipeline_host_resident = pipeline_bytes
        pipeline_host_transient = pipeline_bytes
        decode_weights_parked = vae_bytes
        parked_ram = 0
    else:  # pragma: no cover - normalization already rejected everything else
        raise ValueError(f"Неизвестный режим размещения: {placement!r}")

    # The encoder now always runs in HOST memory (see `_encode_prompts_locked`),
    # and it does so while the pipeline is already placed — so this phase carries
    # the pipeline's residency on both sides, but computes nothing on the device.
    # `pipeline_device` is therefore both the encode phase's whole device cost and
    # the denoise phase's weight term: the same weights, sitting still.
    encode_vram = pipeline_device
    encode_ram = text_encoder_bytes + ENCODE_ACTIVATION_BYTES + pipeline_host_resident

    decode_weights = (
        decode_weights_parked if normalized["unload_transformer_before_vae"] else pipeline_device
    )
    # The pipeline's transient host copy and the resident encoder never overlap
    # under the non-offload placements — the encoder is read only after the
    # transformer has left the host — so the host term is their maximum. Under
    # the offload placements `pipeline_host_transient == pipeline_host_resident`,
    # and the maximum is the sum it has to be there.
    run_ram = max(pipeline_host_transient, pipeline_host_resident + resident_encoder)
    if not encoder_installed:
        # Both encode phases are impossible here (see `encoder_installed` above),
        # so they cost nothing at all — not even the activation scratch of an
        # encode that will never happen. Leaving them at their nominal cost would
        # make `estimate` report a peak for work this machine cannot perform, and
        # `_preset_advice` weigh candidates against it.
        encode_vram = 0
        encode_ram = 0
    phases = {
        "encode": {"vram_bytes": int(encode_vram), "ram_bytes": int(encode_ram)},
        # The same encode WITHOUT a pipeline: what
        # `inpaint.flux2_klein.prompt_cache.build` costs. It reads the encoder
        # and nothing else, so it carries neither the placed weights on the card
        # nor the pipeline's host copy. Its device cost is zero and its host cost
        # is never above the `encode` phase's, so adding it here cannot move
        # `vram_bytes` / `ram_bytes`, `estimate` or `_preset_fits` — it only
        # gives the guard a phase to check for a build.
        "encode_standalone": {
            "vram_bytes": 0,
            "ram_bytes": int(text_encoder_bytes + ENCODE_ACTIVATION_BYTES)
            if encoder_installed
            else 0,
        },
        "denoise": {
            "vram_bytes": int(pipeline_device + denoise_activations),
            "ram_bytes": int(run_ram),
        },
        "decode": {
            "vram_bytes": int(decode_weights + decode_activations),
            "ram_bytes": int(pipeline_host_resident + resident_encoder + parked_ram),
        },
    }
    return {
        "vram_bytes": max(phase["vram_bytes"] for phase in phases.values()),
        "ram_bytes": max(phase["ram_bytes"] for phase in phases.values()),
        "phases": phases,
        # The two costs a repeat request can arrive with already paid: the placed
        # pipeline on the card, and the kept text encoder in host memory. The
        # guard subtracts whichever the service is actually holding — see
        # `_require_memory_headroom`. They are published here, and not
        # recomputed there, so there stays exactly ONE calculation in this module.
        "resident": {
            "pipeline_device": int(pipeline_device),
            "text_encoder_host": int(text_encoder_bytes),
        },
        "breakdown": {
            "transformer": int(transformer_bytes),
            "text_encoder": int(text_encoder_bytes),
            "vae": int(vae_bytes),
            "activations": int(denoise_activations + decode_activations),
            # The encode phase is now the one phase whose cost is dominated by
            # HOST memory (the 16 GB encoder) while it also holds the pipeline on
            # the card, so its peak is the larger of the two sides rather than
            # "the VRAM figure, or the RAM one when there is no VRAM cost".
            "peak_encode": max(
                phases["encode"]["vram_bytes"], phases["encode"]["ram_bytes"]
            ),
            "peak_denoise": phases["denoise"]["vram_bytes"],
            "peak_decode": phases["decode"]["vram_bytes"],
        },
    }


#: Human-readable phase names for the guard's message.
_PHASE_LABELS = {
    "encode": "кодирование промпта",
    "encode_standalone": "кодирование промпта (кэширование)",
    "denoise": "денойз",
    "decode": "декодирование VAE",
}


def _require_memory_headroom(
    normalized: dict[str, Any],
    region_width: int,
    region_height: int,
    device: str,
    *,
    phases: tuple[str, ...],
    pipeline_resident: bool = False,
    encoder_resident: bool = False,
) -> None:
    """Refuse the run when a phase's forecast does not fit in the free memory.

    Called BEFORE the first component is read, because the failure this prevents
    is not an exception: a 9B transformer and an 8B encoder that do not fit make
    the kernel's OOM killer pick a victim, and the victim is whatever else the
    user has open — it has already cost one editor session with unsaved work.
    A `torch.OutOfMemoryError` we could have caught is the good case; the host
    side has no such thing.

    `phases` names the phases that will actually load something on this request:
    a cached prompt skips `encode`, a resident pipeline skips `denoise`/`decode`.
    Each listed phase is checked SEPARATELY, because they run one after another
    and a run is limited by its largest phase, not by their sum.

    `pipeline_resident` / `encoder_resident` say what the service is ALREADY
    holding. Their cost is subtracted from every phase, because the free-memory
    figures this compares against already exclude it: the placed pipeline's VRAM
    and the kept encoder's RAM are allocated, not pending. Without the discount a
    repeat request with a new prompt is refused for memory it is already sitting
    on — measured on this project's reference host, where the second prompt to a
    resident pipeline was told it needed 17.6 GiB of VRAM that the very same
    pipeline was occupying. The discounted figures are what the message reports,
    so the numbers the user sees are the ones that were compared.

    `device` is the resolved torch device string, so the VRAM figures come from
    the card the run would actually use. A memory figure reported as `0` (no
    psutil, no accelerator) is unknown, not zero, and never refuses a run.

    # Raises
    `RuntimeError` naming the short resource, the phase that needs it, how much
    it needs, how much is free, and which settings do fit right now.
    """
    if not phases:
        return
    forecast = forecast_memory(normalized, region_width, region_height)
    memory = hardware.memory_snapshot(device)
    held_vram = forecast["resident"]["pipeline_device"] if pipeline_resident else 0
    held_ram = forecast["resident"]["text_encoder_host"] if encoder_resident else 0

    short: list[str] = []
    for phase in phases:
        cost = forecast["phases"][phase]
        label = _PHASE_LABELS[phase]
        ram_cost = max(cost["ram_bytes"] - held_ram, 0)
        vram_cost = max(cost["vram_bytes"] - held_vram, 0)
        ram_need = ram_cost + HOST_MEMORY_RESERVE_BYTES
        vram_need = vram_cost + DEVICE_MEMORY_RESERVE_BYTES
        if ram_cost and not _fits(ram_need, memory["ram_free"]):
            short.append(
                f"оперативной памяти на этап «{label}»: нужно {_gib(ram_need)} (прогноз "
                f"{_gib(ram_cost)} + резерв {_gib(HOST_MEMORY_RESERVE_BYTES)}), "
                f"свободно {_gib(memory['ram_free'])}"
            )
        if vram_cost and not _fits(vram_need, memory["vram_free"]):
            short.append(
                f"видеопамяти на {device} на этап «{label}»: нужно {_gib(vram_need)} (прогноз "
                f"{_gib(vram_cost)} + резерв {_gib(DEVICE_MEMORY_RESERVE_BYTES)}), "
                f"свободно {_gib(memory['vram_free'])}"
            )
    if not short:
        return

    log.error(
        "FLUX.2 klein: запуск отклонён до чтения весов — %s (режим «%s», область %dx%d).",
        "; ".join(short),
        normalized["placement"],
        int(region_width),
        int(region_height),
    )
    raise RuntimeError(
        f"Недостаточно {'; '.join(short)}. Загрузка не начата, чтобы система не осталась без "
        f"памяти. {_preset_advice(normalized, region_width, region_height, memory)}"
    )


def _preset_advice(
    normalized: dict[str, Any], region_width: int, region_height: int, memory: dict[str, int]
) -> str:
    """One sentence naming the settings whose forecast fits `memory` right now.

    Computed rather than hard-coded, so the advice cannot recommend the very
    preset that just failed. Besides the four presets it considers one separate
    lever: switching OFF `unload_transformer_before_vae`. Parking a 9B
    transformer copies it into host memory, so that single flag can be the whole
    difference on a machine that is short of RAM — and the decode's own recovery
    ladder still parks lazily if the VAE actually runs out of VRAM. When nothing
    fits, the region is the only remaining lever and the message says so.

    Deliberately undiscounted, unlike the guard itself: switching preset changes
    the model key, so the resident pipeline is evicted before the new one loads
    and the candidate really does start from the free memory measured here. The
    cost is that a "current mode without X" candidate — which keeps the key, and
    therefore the residency — is judged more strictly than it would run. That
    errs towards offering fewer options than exist, never towards offering one
    that would fail.
    """
    candidates: list[tuple[str, dict[str, Any]]] = []
    for label, placement, low_cpu in _MEMORY_PRESETS:
        # The preset owns `unload_transformer_before_vae` too, but that one flag
        # is worth offering separately: it is the difference between keeping the
        # transformer on the device and copying all 9B of it back into host
        # memory, so a preset that does not fit with it often fits without.
        for parked in (placement != "full_gpu", False):
            suffix = "" if parked else " без выгрузки трансформера перед VAE"
            candidates.append(
                (
                    f"{label}{suffix}",
                    {
                        "placement": placement,
                        "low_cpu_mem_usage": low_cpu,
                        "unload_transformer_before_vae": parked,
                        # A preset carries the shipped default, which is now to
                        # KEEP the encoder in host memory in every placement.
                        "unload_text_encoder_after_encode": False,
                    },
                )
            )
    # Dropping the encoder after the encode is its own lever: it is the shipped
    # default no longer, so on a host that is short of RAM it is the first thing
    # to offer — 16 GB back, at the price of re-reading the encoder on the next
    # prompt that misses the cache.
    if not normalized["unload_text_encoder_after_encode"]:
        candidates.append(
            (
                "текущий режим с выгрузкой энкодера после кодирования",
                {"unload_text_encoder_after_encode": True},
            )
        )
    # Only when the user is on a custom combination: for a preset the same
    # advice is already in the list under the preset's own name.
    current = (normalized["placement"], bool(normalized["low_cpu_mem_usage"]))
    if normalized["unload_transformer_before_vae"] and current not in {
        (placement, low_cpu) for _label, placement, low_cpu in _MEMORY_PRESETS
    }:
        candidates.append(
            (
                "текущий режим без выгрузки трансформера перед VAE",
                {"unload_transformer_before_vae": False},
            )
        )

    fitting: list[str] = []
    for label, overrides in candidates:
        if label in fitting:
            continue
        candidate = dict(normalized)
        candidate.update(overrides)
        if _preset_fits(candidate, region_width, region_height, memory):
            fitting.append(label)
    if not fitting:
        return (
            "Ни один из встроенных профилей памяти сейчас не помещается: уменьшите выделенную "
            "область или освободите память, закрыв другие программы."
        )
    return "Сейчас помещаются: " + ", ".join(f"«{label}»" for label in fitting) + "."


def _preset_fits(
    candidate: dict[str, Any], region_width: int, region_height: int, memory: dict[str, int]
) -> bool:
    """Whether every phase of one preset fits the free memory, reserves included."""
    forecast = forecast_memory(candidate, region_width, region_height)
    for cost in forecast["phases"].values():
        if cost["ram_bytes"] and not _fits(
            cost["ram_bytes"] + HOST_MEMORY_RESERVE_BYTES, memory["ram_free"]
        ):
            return False
        if cost["vram_bytes"] and not _fits(
            cost["vram_bytes"] + DEVICE_MEMORY_RESERVE_BYTES, memory["vram_free"]
        ):
            return False
    return True


def _gib(value: int) -> str:
    """Bytes as a human-readable GiB figure for a user-facing message."""
    return f"{int(value) / (1024**3):.1f} ГиБ"


def _fits(required: float, free: int) -> bool:
    """Whether `required` bytes fit in `free`; unknown (`0`) free memory passes."""
    return True if free <= 0 else int(required) <= int(free)
