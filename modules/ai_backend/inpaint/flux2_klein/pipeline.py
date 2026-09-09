"""
File: modules/ai_backend/inpaint/flux2_klein/pipeline.py

Purpose:
Building the `Flux2KleinInpaintPipeline` out of the five components and keeping
its weights where the chosen placement says they belong. Everything in this file
operates on a pipeline or a component module; nothing here owns the service lock
or the model lease - that is `service.py`.

Main responsibilities:
- component loaders (`_load_transformer`, `_load_text_encoder`, `_load_vae`),
  the single-file transformer's config contract, the choice between the
  tensor-at-a-time loader of `streaming.py` and diffusers' own
  `from_single_file`, and the text encoder's
  truncation to `components.ENCODER_KEEP_LAYERS` decoder layers (with
  `_quiet_transformers_load_warnings` around that one load);
- placement (`_apply_placement`, `_require_components_materialized`,
  `_materialize_components_for_offload`) and the ROCm staging patch it runs under;
- per-component residency and the action matrix the client renders
  (`_module_residency`, `_component_actions`, plus the name validators);
- the warm-up decode, the prompt-encode phase and the fp8 quantization;
- the VAE decode with its out-of-memory recovery (`_decode_region_latents`,
  `_park_transformer_off_device`, `_restore_transformer_to_device`), including
  the tiling gate that says whether tiling can engage at all
  (`_vae_tiling_engages`) and the last-resort tile shrink
  (`_lowered_vae_tile_thresholds`, whose two thresholds must move together).

Notes:
- `patched_module_to`, `mmap_staging_required`, `tensor_needs_staging`,
  `_quantize_text_encoder_fp8` and `_restore_transformer_to_device` are replaced
  by the test suite through this module object; `service.py` therefore calls them
  as `pipeline.<name>(...)`.
- `streaming.load_transformer_streaming` / `streaming.streaming_load_eligible`
  are reached through the `streaming` module for the same reason.
- `hardware._clear_torch_cache` / `hardware.memory_snapshot` are reached through
  their module for the same reason.
- torch / diffusers / transformers are imported lazily inside the functions that
  need them.
"""

from __future__ import annotations

import contextlib
import logging
import time
from pathlib import Path
from typing import Any, Callable, Iterator

from ...runtime.rocm_mmap_transfer import (
    mmap_staging_required,
    patched_module_to,
    tensor_needs_staging,
)
from . import hardware, streaming
from .components import (
    _fp8_scaled_message,
    _missing_transformer_config_message,
    _reject_fp8_scaled_directory,
    component_dir_for_path,
    find_transformer_config_dir,
    is_fp8_scaled_checkpoint,
    read_safetensors_header,
    text_encoder_truncation_kwargs,
    TEXT_ENCODER_OUT_LAYER_INDICES,
    validate_transformer_config_dir,
)
from .progress import FileProgressCb

log = logging.getLogger(__name__)

#: Spatial size of the warm-up latent handed to the VAE, in latent cells. The
#: decode it triggers is what materializes the placed weights and initializes the
#: allocator and (on ROCm) the MIOpen convolution kernels, so it must be a REAL
#: forward — but it costs `WARMUP_LATENT_CELLS * vae_scale_factor` pixels, i.e. a
#: 64x64 image, which is nothing next to the region itself.
WARMUP_LATENT_CELLS = 8

#: Latent tile side the LAST rung of the decode's OOM ladder falls back to
#: (`_decode_region_latents`, step 3). It is diffusers' own working assumption —
#: `tiled_decode` documents itself as splitting `z` into "overlapping 64x64 tiles"
#: — and on a four-block VAE it means a 512 px tile, a quarter of the shipped
#: 1024 px threshold's area. It is used only as an emergency, per decode, and is
#: restored afterwards; see `_lowered_vae_tile_thresholds`.
EMERGENCY_TILE_LATENT_MIN_SIZE = 64

#: Components loaded from safetensors, i.e. the ones diffusers hands out backed
#: by a writable private file mapping and which therefore hit the ROCm amdkfd
#: stall. The transformer is LAST on purpose: it is by far the largest, so a
#: failed (OOM) round trip of it still leaves the smaller one re-homed. The text
#: encoder is absent because it is not part of the pipeline any more — the prompt
#: phase loads it after this one is placed, and it never goes on the device.
_MMAP_BACKED_COMPONENTS = ("vae", "transformer")


# ---------------------------------------------------------------------------
# Per-component residency and actions (dev-docs/flux2_component_residency.md)
# ---------------------------------------------------------------------------
# Three labels ("not loaded" / "in RAM" / "on the GPU") are not enough to be
# honest here: under `sequential_cpu_offload` accelerate moves the parameters to
# the `meta` device and keeps the bytes in a host weights map, so the component
# is in neither place, and a load can leave part of a component behind — which is
# exactly what `_require_components_materialized` already exists to catch.

#: The component does not exist in the service right now.
RESIDENCY_NOT_LOADED = "not_loaded"
#: Every parameter and buffer is on the host.
RESIDENCY_RAM = "ram"
#: Every parameter and buffer is on the compute device.
RESIDENCY_GPU = "gpu"
#: An accelerate hook owns it: the parameters sit on `meta` (or are pulled back
#: and forth around each forward) and the bytes live in a host weights map.
RESIDENCY_OFFLOADED = "offloaded"
#: Genuinely split across devices. REPORTED, never rounded to a neighbouring
#: state: rounding is how a half-placed pipeline turns into a device mismatch
#: several frames inside the VAE instead of a named problem.
RESIDENCY_MIXED = "mixed"

#: The three weight-bearing components a client can ask about or act on. The
#: tokenizer and the scheduler carry no weights and are absent on purpose.
ACTIONABLE_COMPONENTS = ("text_encoder", "transformer", "vae")

#: Every action name the wire accepts, in the order `actions` lists them, so the
#: UI's button order is decided here and not re-derived per client.
COMPONENT_ACTIONS = ("load", "unload", "to_ram", "to_gpu", "warmup")

#: Actions that run under a `LoadedModelManager` lease. Only `load` does: it is
#: the one action that can make a NEW key resident, which is the whole of what a
#: lease accounts for. `unload` and `to_ram` only release. `to_gpu` and `warmup`
#: act on a pipeline that is already resident under `_active_key` and change no
#: key, and a lease there would be worse than useless: an eviction cannot race
#: them anyway (the eviction callback `_unload_key` takes `self._lock`, which the
#: action holds throughout), while a lease whose load never happens has to be
#: aborted by hand or its manager entry stays `loading` forever and the next
#: request for that key waits on it.
_LEASED_ACTIONS = ("load",)


# =====================================================================
#  Pipeline construction helpers
# =====================================================================
def _load_transformer(
    model_cls: Any,
    path: str,
    *,
    dtype: Any,
    device_map: dict[str, str] | None,
    low_cpu_mem_usage: bool,
    progress: FileProgressCb | None = None,
) -> Any:
    """Load `Flux2Transformer2DModel` from a diffusers folder or a single file.

    Two things hold for BOTH inputs and are handled before the branch:
    - klein has no `guidance_in` block, so `guidance_embeds=False` must override
      whatever config is resolved — a diffusers folder carrying the flux-2-dev
      value of `true` would otherwise build the wrong architecture;
    - an fp8_scaled checkpoint is refused up front, sharded folders included, so
      the failure costs a header read instead of a partial multi-GiB load.

    A single file additionally goes through `from_single_file`, where diffusers
    detects every flux2 checkpoint as `flux-2-dev` and would fetch that (gated,
    and simply DIFFERENT) repo's config from the Hub. That path is blocked: a
    local config directory must be found next to the checkpoint, otherwise the
    load is refused. There is no reconstruction of the config from the weights —
    `rope_theta`, `eps` and `patch_size` do not follow from any tensor shape, and
    a model built with the wrong `rope_theta` has exactly the same shapes and
    quietly produces wrong images.

    The two loaders also take the target device through DIFFERENT kwargs, which
    is why `device_map` is only forwarded to the directory branch; see
    `_single_file_device`.

    **A single file is read TENSOR BY TENSOR when it can be**
    (`streaming.load_transformer_streaming`): `from_single_file` materializes the
    WHOLE checkpoint in host memory before anything reaches the accelerator, so
    on a ~17 GiB klein checkpoint the host peak is the checkpoint itself. The
    decision is `streaming.streaming_load_eligible` — that module carries no
    fallback on purpose — and a refusal is LOGGED here, at the site that makes
    the choice, before the ordinary loader runs. `progress` is the byte-level
    channel the streaming loader drives; it is ignored on every other path,
    because nothing else can report bytes.

    `streaming.load_transformer_streaming` runs OUTSIDE `patched_module_to()`
    deliberately: its tensors come from `torch.frombuffer` over anonymous memory,
    which reports `tensor_needs_staging() == False`, it never calls
    `nn.Module.to`, and that patch's contract forbids file I/O inside the block.
    Placement is not weakened by any of this — `_apply_placement` still moves the
    components unconditionally afterwards, and `_require_components_materialized`
    still checks the result.

    # Raises
    `ValueError` for an fp8_scaled checkpoint (unsupported by diffusers 0.39) or
    for a config that belongs to another model class; `FileNotFoundError` when a
    single-file checkpoint has no transformer config next to it.
    """
    source = Path(path)
    kwargs: dict[str, Any] = {"torch_dtype": dtype, "low_cpu_mem_usage": low_cpu_mem_usage}
    # `guidance_embeds` lands in the model kwargs that override the resolved
    # config; klein has no guidance embedder. It belongs to both loaders, so it
    # is set before the directory branch returns.
    kwargs["guidance_embeds"] = False

    if source.is_dir():
        _reject_fp8_scaled_directory(source)
        if device_map is not None:
            kwargs["device_map"] = device_map
        return model_cls.from_pretrained(str(source), **kwargs)

    header = read_safetensors_header(source)
    if is_fp8_scaled_checkpoint(header):
        raise ValueError(_fp8_scaled_message(source))

    config_dir = find_transformer_config_dir(source)
    if config_dir is None:
        raise FileNotFoundError(_missing_transformer_config_message(source))
    validate_transformer_config_dir(config_dir)

    # The streaming loader keeps the host peak at ONE TENSOR instead of the whole
    # checkpoint. It refuses by name rather than falling back, so the choice —
    # and the reason for not taking it — is made and logged here.
    eligible, reason = streaming.streaming_load_eligible(
        source, low_cpu_mem_usage=low_cpu_mem_usage, device_map=device_map
    )
    if eligible:
        return streaming.load_transformer_streaming(
            model_cls,
            source,
            dtype=dtype,
            device_map=device_map,
            low_cpu_mem_usage=low_cpu_mem_usage,
            # The loader names the TENSOR it just read; the wire's second level
            # names the FILE, which `progress` already has bound, so the key is
            # dropped here rather than flickering through the user's progress bar.
            progress=None if progress is None else lambda done, total, _key: progress(done, total),
        )
    log.info(
        "FLUX.2 klein: потоковая загрузка трансформера не применяется (%s) — "
        "используется обычный загрузчик diffusers.",
        reason,
    )

    # `config` as a local directory is what keeps the loader off the Hub: with it
    # set, diffusers never calls `fetch_diffusers_config`, which is where the
    # flux-2-dev repo id comes from. `local_files_only` closes the door a second
    # time, so a future loader change cannot silently reopen the network path.
    kwargs["config"] = str(config_dir)
    kwargs["local_files_only"] = True
    single_file_device = _single_file_device(device_map)
    if single_file_device is not None:
        kwargs["device"] = single_file_device
    log.info("FLUX.2 klein: конфиг трансформера взят из %s", config_dir)
    return model_cls.from_single_file(str(source), **kwargs)


def _single_file_device(device_map: dict[str, str] | None) -> str | None:
    """Translate a whole-model `device_map` into the kwarg `from_single_file` honours.

    diffusers 0.39's single-file loader accepts `device_map`, pops it, and then
    throws it away: `single_file_model.py` reassigns `device_map = None` right
    before the `low_cpu_mem_usage` branch and rebuilds it from its own separate
    `device` kwarg, which defaults to the CPU. Passing `device_map` there is
    therefore silently ignored — an 18 GB transformer stays in host RAM while the
    caller believes it is on the accelerator. Returns the single device the map
    names, or `None` when there is no map or it is not one whole-model entry (in
    which case the caller must place the weights itself).
    """
    if not device_map:
        return None
    if set(device_map) != {""}:
        return None
    return str(device_map[""])


#: Loggers a truncated encoder load must not shout through. transformers reports
#: every checkpoint key it did not consume, and a truncated load leaves a whole
#: block of decoder layers unconsumed by design, so the warning is a wall of
#: noise about the exact thing we asked for.
_TRANSFORMERS_LOAD_LOGGERS = ("transformers.modeling_utils",)


@contextlib.contextmanager
def _quiet_transformers_load_warnings() -> Iterator[None]:
    """Silence transformers' UNEXPECTED-key warnings for the duration of ONE load.

    Used only around a load that was DELIBERATELY truncated
    (`text_encoder_truncation_kwargs`), where "some weights of the checkpoint
    were not used" is the intended outcome and printing several hundred tensor
    names would bury the load steps the user is actually watching. Levels are
    restored in `finally`, so an exception inside the load cannot leave the
    process with transformers muted.
    """
    restore: list[tuple[logging.Logger, int]] = []
    for name in _TRANSFORMERS_LOAD_LOGGERS:
        logger = logging.getLogger(name)
        restore.append((logger, logger.level))
        logger.setLevel(logging.ERROR)
    try:
        yield
    finally:
        for logger, level in restore:
            logger.setLevel(level)


def _load_text_encoder(
    model_cls: Any,
    path: str,
    *,
    dtype: Any,
    device_map: dict[str, str] | None,
    low_cpu_mem_usage: bool,
    keep_layers: int | None,
) -> Any:
    """Load the Qwen3 text encoder from a transformers/diffusers folder.

    A file path pointing INTO such a folder is accepted and normalized to it;
    see `component_dir_for_path`.

    The dtype goes through `dtype=`, not `torch_dtype=`: this is the one
    transformers loader here, and transformers 4.57 deprecated the old spelling
    (`modeling_utils.py`, "`torch_dtype` is deprecated! Use `dtype` instead!").
    The diffusers loaders in this module keep `torch_dtype` — diffusers 0.39
    recognizes nothing else.

    `keep_layers` is how many decoder layers the loaded model must have, i.e.
    `components.ENCODER_KEEP_LAYERS` for a run; `None` loads the checkpoint whole
    and exists for a caller that is not encoding a klein prompt. It is applied
    through `text_encoder_truncation_kwargs`, which decides per config what to
    pass and refuses an encoder with too few layers, and the load it produces is
    the only one whose "unused checkpoint keys" warnings are suppressed — those
    keys are the layers we asked not to build.

    # Raises
    `ValueError` when `path` is not a transformers folder, or when the encoder
    has fewer layers than the pipeline reads.
    """
    source = component_dir_for_path(Path(path))
    if not source.is_dir():
        raise ValueError(
            f"Путь текстового энкодера должен быть каталогом в формате transformers: {source}"
        )
    kwargs: dict[str, Any] = {"dtype": dtype, "low_cpu_mem_usage": low_cpu_mem_usage}
    if device_map is not None:
        kwargs["device_map"] = device_map
    truncation = (
        text_encoder_truncation_kwargs(source, keep_layers) if keep_layers is not None else {}
    )
    kwargs.update(truncation)
    if not truncation:
        return model_cls.from_pretrained(str(source), **kwargs)
    with _quiet_transformers_load_warnings():
        return model_cls.from_pretrained(str(source), **kwargs)


def _load_vae(
    model_cls: Any,
    path: str,
    *,
    dtype: Any,
    device_map: dict[str, str] | None,
    low_cpu_mem_usage: bool,
) -> Any:
    """Load `AutoencoderKLFlux2` from a diffusers folder (or try a single file).

    diffusers 0.39 registers no single-file mapping for this class. A file that
    sits next to a `config.json` is normalized to its folder
    (`component_dir_for_path`), which is what a user selecting
    `diffusion_pytorch_model.safetensors` means. A file without that sibling is
    still attempted, and the loader failure is translated into an actionable
    message rather than surfacing as an internal error.
    """
    source = component_dir_for_path(Path(path))
    kwargs: dict[str, Any] = {"torch_dtype": dtype, "low_cpu_mem_usage": low_cpu_mem_usage}
    if device_map is not None:
        kwargs["device_map"] = device_map
    if source.is_dir():
        return model_cls.from_pretrained(str(source), **kwargs)
    try:
        return model_cls.from_single_file(str(source), **kwargs)
    except Exception as exc:
        raise ValueError(
            f"Не удалось загрузить VAE из файла {source.name}: {exc}. Укажите каталог VAE в "
            "формате diffusers (config.json + diffusion_pytorch_model.safetensors)."
        ) from exc


def _apply_vae_memory_options(pipe: Any, options: dict[str, Any]) -> None:
    """Enable/disable VAE tiling and slicing to match `options`.

    `options` is any mapping carrying `vae_tiling` / `vae_slicing` — the
    normalized params, or the `applied` dict during OOM recovery. Applied on
    every request, including a cache hit, so toggling the option in the UI takes
    effect without rebuilding the pipeline.

    **Both flags are REQUESTS, and neither changes a threshold.** `enable_tiling`
    only sets `use_tiling`; `AutoencoderKLFlux2._decode` still enters the tiled
    path only when a latent side exceeds `tile_latent_min_size` (1024 output px
    on the shipped klein VAE), so on a smaller region this call saves nothing.
    `enable_slicing` is inert here in EVERY case: `decode` slices only when
    `z.shape[0] > 1` and this service always decodes a batch of one. It stays on
    the wire because the Rust `MemoryPreset` values mirror it — see
    `dev-docs/known_gaps.md`. Whoever needs to know whether tiling will really
    engage asks `_vae_tiling_engages`, never this function.
    """
    vae = getattr(pipe, "vae", None)
    if vae is None:
        return
    for enabled, enable_name, disable_name in (
        (options["vae_tiling"], "enable_tiling", "disable_tiling"),
        (options["vae_slicing"], "enable_slicing", "disable_slicing"),
    ):
        method = getattr(vae, enable_name if enabled else disable_name, None)
        if callable(method):
            method()


def _apply_placement(pipe: Any, placement: str, device: Any) -> None:
    """Put the pipeline's weights where `placement` says they belong.

    The move is UNCONDITIONAL for the two non-offload placements, even when the
    components were asked to load straight onto `device`: `nn.Module.to` is a
    no-op per tensor that is already there, so the cost of being sure is nil,
    while trusting the loader kwargs is not safe. diffusers 0.39's
    `from_single_file` accepts `device_map` and discards it (see
    `_single_file_device`), which used to leave the transformer in host memory
    while this function skipped its move — the pipeline then reported `cpu` as
    its execution device and the run died inside the VAE's first `conv2d` with a
    CPU input against CUDA weights.

    Every move is wrapped in `patched_module_to()`, whose contract allows the
    weight transfer and nothing else inside the block.
    """
    if placement == "full_gpu":
        with patched_module_to():
            pipe.to(device)
    elif placement == "encoder_cpu":
        # Named for the era when the pipeline still carried the text encoder. It
        # does not any more, so this moves exactly what `full_gpu` moves — the
        # two components are the whole pipeline — and the two branches stay
        # separate only because a component-wise move is what makes
        # `_require_execution_device`'s per-component report meaningful when a
        # loader has ignored its placement kwarg.
        with patched_module_to():
            pipe.transformer.to(device)
            pipe.vae.to(device)
    elif placement == "model_cpu_offload":
        _materialize_components_for_offload(pipe, device)
        pipe.enable_model_cpu_offload(device=str(device))
    elif placement == "sequential_cpu_offload":
        _materialize_components_for_offload(pipe, device)
        pipe.enable_sequential_cpu_offload(device=str(device))
    else:  # pragma: no cover - normalization already rejected everything else
        raise ValueError(f"Неизвестный режим размещения: {placement!r}")


#: Components whose weights must be on the accelerator once placement is done.
#: The tokenizer and the scheduler carry no tensors, and the text encoder is not
#: part of the pipeline at all.
_PLACED_COMPONENTS = ("transformer", "vae")


def _require_components_materialized(pipe: Any, device: Any) -> None:
    """Refuse a placement that only LOOKS done: weights still on host or `meta`.

    Called right after `_apply_placement`, before the text encoder is read. It
    exists because "placed" and "materialized" are different claims and this
    module has already been bitten by the difference twice: diffusers 0.39's
    `from_single_file` accepts `device_map` and discards it, and accelerate's
    `device_map` path can leave a parameter on `meta` when a shard fails to map.
    Either way the run dies much later, several frames inside the VAE, naming
    neither the component nor the placement — and, worse for the new load order,
    the host memory the text encoder is about to need was never actually freed.

    Compares device TYPES, not full specs: `cuda` and `cuda:0` are the same card
    for this purpose, and the ordinal is already fixed by `_apply_placement`. A
    component with no `parameters()` (a scheduler, a test double) is skipped.

    # Raises
    `RuntimeError` naming each component that is not fully on `device`, with the
    number of offending tensors and the devices they sit on.
    """
    target = getattr(device, "type", None) or str(device or "").split(":")[0]
    if not target:
        return
    offenders: list[str] = []
    for name in _PLACED_COMPONENTS:
        module = getattr(pipe, name, None)
        parameters = getattr(module, "parameters", None)
        buffers = getattr(module, "buffers", None)
        if not callable(parameters) or not callable(buffers):
            continue
        strays: dict[str, int] = {}
        for tensor in list(parameters()) + list(buffers()):
            where = getattr(getattr(tensor, "device", None), "type", None)
            if where is not None and where != target:
                strays[where] = strays.get(where, 0) + 1
        if strays:
            detail = ", ".join(f"{count} на «{where}»" for where, count in sorted(strays.items()))
            offenders.append(f"{name}: {detail}")
    if not offenders:
        return
    raise RuntimeError(
        f"FLUX.2 klein: после размещения часть весов осталась не на «{device}» — {'; '.join(offenders)}. "
        "Запуск остановлен здесь, а не внутри VAE: загрузчик проигнорировал параметр размещения, "
        "и оперативная память, которая нужна текстовому энкодеру следующим шагом, не освободилась."
    )


def _module_residency(module: Any) -> str:
    """Where `module`'s weights actually are, as one `RESIDENCY_*` wire literal.

    This is the honest answer the FLUX.2 parameter panel shows per component, and
    it deliberately reuses the two probes this file already relies on instead of
    inventing a third:

    1. `hasattr(module, "_hf_hook")` — an accelerate hook owns the module, which
       is how diffusers itself discriminates the offload placements
       (`pipeline_utils` checks the hook class). The answer is then `offloaded`
       whatever the parameters currently say: under `model_cpu_offload` they sit
       on the host between forwards and on the accelerator during one, and
       reporting that momentary truth as `ram`/`gpu` would be a value the user
       cannot act on.
    2. the device-TYPE count over `parameters()` + `buffers()`, exactly as
       `_require_components_materialized` does it. Types, not full specs:
       `cuda` and `cuda:0` are the same card here.

    `meta` without a hook is `offloaded` too: `accelerate.cpu_offload` moves the
    parameters to `meta` and keeps the bytes in a host weights map, and the hook
    it leaves behind may sit on a submodule rather than on the component root.

    More than one device type is `mixed` and is returned as such. A module with
    no parameters and no buffers holds no weights anywhere, so it answers
    `not_loaded` — that is also what a test double answers, which is correct
    rather than merely convenient.

    `None` (no such component) answers `not_loaded`.
    """
    if module is None:
        return RESIDENCY_NOT_LOADED
    if hasattr(module, "_hf_hook"):
        return RESIDENCY_OFFLOADED

    parameters = getattr(module, "parameters", None)
    buffers = getattr(module, "buffers", None)
    if not callable(parameters) or not callable(buffers):
        return RESIDENCY_NOT_LOADED

    kinds: set[str] = set()
    for tensor in list(parameters()) + list(buffers()):
        where = getattr(getattr(tensor, "device", None), "type", None)
        if where:
            kinds.add(str(where))
    if not kinds:
        return RESIDENCY_NOT_LOADED
    if len(kinds) > 1:
        return RESIDENCY_MIXED
    only = next(iter(kinds))
    if only == "meta":
        return RESIDENCY_OFFLOADED
    if only == "cpu":
        return RESIDENCY_RAM
    return RESIDENCY_GPU


def _component_actions(component: str, residency: str, *, pipeline_loaded: bool) -> list[str]:
    """The actions that are genuinely possible for `component` right now.

    This list is the AUTHORITY the client renders: the rule lives here and
    nowhere else, because a matrix expressed twice in two languages drifts on the
    first edit. Ordered by `COMPONENT_ACTIONS`, so the button order is decided
    here too.

    The invariants of `dev-docs/flux2_component_residency.md` §3, and why:

    - the text encoder never offers `to_gpu`. `_encode_prompts_locked` pins
      `torch.device("cpu")` unconditionally and the load-order memory contract
      depends on it — 18.3 GB of transformer plus 16.4 GB of encoder do not fit
      on this project's 34.2 GB reference card.
    - the transformer never offers `warmup`. There is no such path: the warm-up
      forwards the VAE only (`_warmup_vae_decode`), and building a transformer
      forward just to have a button would be a fake.
    - `load` and `unload` appear on BOTH the transformer and the VAE, always
      together, because they act on the PIPELINE as a whole. Per-component
      load/unload of one of them is not safe today: `_model_key` describes a
      whole pipeline, so dropping one component would make the next request
      cache-hit onto a broken pipeline, and `forecast_memory`'s `resident`
      discount cannot represent a half-loaded one.
    - the VAE offers no `to_ram`/`to_gpu`. No helper exists for it, and a
      host-resident VAE is not a memory win the user can see: `_decode_once`
      follows `vae.device`, so the next decode would silently run on the CPU.
    - a transformer that is `offloaded` or `mixed` offers no move either. Under
      accelerate the move must be ABSENT rather than disabled (meta tensors, and
      the hooks would have to be removed first), and `.to(device)` on a mixed
      module raises "Cannot copy out of meta tensor" as often as it succeeds.
    """
    if component == "text_encoder":
        # The encoder is not a pipeline component here, so it loads and unloads
        # on its own; `pipeline_loaded` says nothing about it.
        return ["load"] if residency == RESIDENCY_NOT_LOADED else ["unload"]

    if not pipeline_loaded:
        return ["load"]

    actions = ["unload"]
    if component == "transformer":
        if residency == RESIDENCY_GPU:
            actions.append("to_ram")
        elif residency == RESIDENCY_RAM:
            actions.append("to_gpu")
    elif component == "vae" and residency == RESIDENCY_GPU:
        # Warming up a VAE that is not materialized on the device is refused by
        # `_require_components_materialized`, so it is not offered there.
        actions.append("warmup")
    return sorted(actions, key=COMPONENT_ACTIONS.index)


def _require_component_name(value: Any) -> str:
    """Validate a wire `component` name against `ACTIONABLE_COMPONENTS`.

    # Raises
    `ValueError` naming the accepted values. The tokenizer and the scheduler are
    rejected here on purpose: they carry no weights, so no residency and no
    action of this family means anything for them.
    """
    name = str(value or "").strip()
    if name in ACTIONABLE_COMPONENTS:
        return name
    accepted = ", ".join(f"«{item}»" for item in ACTIONABLE_COMPONENTS)
    raise ValueError(
        f"FLUX.2 klein: неизвестный компонент «{name}». Допустимые значения: {accepted}."
    )


def _require_action_name(value: Any) -> str:
    """Validate a wire `action` name against `COMPONENT_ACTIONS`.

    # Raises
    `ValueError` naming the accepted values. Whether the action is possible for
    the component RIGHT NOW is a separate question, answered under the service
    lock by `Flux2KleinInpaintService._require_action_available_locked`.
    """
    name = str(value or "").strip()
    if name in COMPONENT_ACTIONS:
        return name
    accepted = ", ".join(f"«{item}»" for item in COMPONENT_ACTIONS)
    raise ValueError(
        f"FLUX.2 klein: неизвестное действие «{name}». Допустимые значения: {accepted}."
    )


def _component_busy_message(component: str, action: str) -> str:
    """The refusal text for an action requested while the service is busy."""
    return (
        f"FLUX.2 klein занят: действие «{action}» над компонентом «{component}» сейчас "
        "невозможно. Генерация или другая операция удерживают модель до своего "
        "завершения — дождитесь её окончания и повторите."
    )


def _warmup_vae_decode(pipe: Any, device: Any) -> bool:
    """Run one tiny VAE decode so the placed weights actually retire on `device`.

    Returns whether the decode ran. The latent is `WARMUP_LATENT_CELLS` cells
    square, i.e. a 64x64 image at this VAE's scale factor — a real forward, but
    a few hundred KiB of activations. It is what turns queued host->device copies
    into completed ones, primes the caching allocator, and on ROCm compiles the
    MIOpen convolution kernels the real decode then reuses.

    When the VAE's config does not name `latent_channels` there is no way to
    synthesize a valid input, so the decode is skipped and only the device
    synchronization is performed — the part that actually governs when the host
    pages are released. That is logged rather than silent, because a klein VAE
    always carries the field and its absence means the component is not what this
    service thinks it is.

    `torch.no_grad()` is explicit for the same reason as in `_decode_once`: this
    call does not go through `pipeline.__call__`, where diffusers puts the
    decorator.
    """
    import torch

    vae = pipe.vae
    channels = getattr(getattr(vae, "config", None), "latent_channels", None)
    if not isinstance(channels, int) or isinstance(channels, bool) or channels <= 0:
        log.warning(
            "FLUX.2 klein: у VAE нет config.latent_channels — прогревочный проход пропущен, "
            "выполнена только синхронизация устройства."
        )
        _synchronize_device(device)
        return False

    with torch.no_grad():
        latents = torch.zeros(
            (1, int(channels), WARMUP_LATENT_CELLS, WARMUP_LATENT_CELLS),
            dtype=vae.dtype,
            device=_vae_input_device(pipe),
        )
        vae.decode(latents, return_dict=False)
    _synchronize_device(device)
    return True


def _synchronize_device(device: Any) -> None:
    """Block until every queued kernel and copy on `device` has retired.

    A host->device weight copy is asynchronous, so `nn.Module.to` can return
    while the source pages are still live in host memory. The new load order
    depends on those pages being gone before the text encoder is read, which is
    what this waits for. A strict no-op on CPU and wherever torch has no
    accelerator.
    """
    kind = getattr(device, "type", None) or str(device or "").split(":")[0]
    if kind != "cuda":
        return
    try:
        import torch

        if torch.cuda.is_available():
            torch.cuda.synchronize()
    except Exception as exc:  # noqa: BLE001 - a missing accelerator must not fail a run
        log.debug("FLUX.2 klein: синхронизация устройства недоступна (%s)", exc)


def _encode_prompt_phase(
    pipeline_cls: Any,
    text_encoder: Any,
    tokenizer: Any,
    prompt: str,
    max_sequence_length: int,
    device: Any,
) -> Any:
    """Encode one prompt with nothing but the encoder loaded; returns CPU embeddings.

    The prompt phase of a run. The encoder is used exactly once and must not be alive at
    the same time as the 9B transformer, so the encoding cannot go through the
    run pipeline — that pipeline does not exist yet. A pipeline instance holding
    ONLY the encoder and its tokenizer is built instead: every other component is
    `None`, which `Flux2KleinInpaintPipeline.__init__` tolerates (its two uses of
    `self.vae` are guarded by `getattr(self, "vae", None)`), and `encode_prompt`
    needs nothing else.

    Going through the pipeline's own `encode_prompt` rather than reimplementing
    it is deliberate: the Qwen3 chat template, the attention mask and the
    stacking of the requested hidden states all live there, and a second copy of
    them would drift from the version diffusers actually denoises with.

    **`text_encoder_out_layers` is passed explicitly, not inherited.** The
    default happens to be `(9, 18, 27)` today, but it is a diffusers default we
    do not own — the sibling `Flux2Pipeline` already uses `(10, 20, 30)` — and
    `components.ENCODER_KEEP_LAYERS` truncates the encoder to exactly the layers
    named here. Inheriting the default would let an upstream edit ask for a layer
    the loaded encoder no longer has.

    The result is moved to the HOST before it is returned, so the prompt cache
    never pins device memory; the caller moves it to the run device.

    `torch.no_grad()` is explicit for the same reason as in `_decode_once`: this
    call does not go through `pipeline.__call__`, which is where diffusers puts
    the decorator, and an 8B forward pass that builds an autograd graph would
    both waste memory and hand the cache tensors that require grad.
    """
    import torch

    holder = pipeline_cls(
        scheduler=None, vae=None, text_encoder=text_encoder, tokenizer=tokenizer, transformer=None
    )
    with torch.no_grad():
        prompt_embeds, _text_ids = holder.encode_prompt(
            prompt=prompt or "",
            device=device,
            max_sequence_length=max_sequence_length,
            text_encoder_out_layers=TEXT_ENCODER_OUT_LAYER_INDICES,
        )
    return prompt_embeds.detach().to("cpu")


def _quantize_text_encoder_fp8(text_encoder: Any) -> int:
    """Replace the encoder's `nn.Linear` weights with float8_e4m3fn + row scales.

    Weight-only quantization done with torch alone — no torchao, bitsandbytes or
    quanto, none of which is installed in this project's environment. Each linear
    keeps a per-output-row scale so one outlier channel cannot flatten the rest,
    and the weight is dequantized to the compute dtype inside `forward`. Measured
    on this project's ROCm host (gfx1201, torch 2.12.0+rocm7.2) the quantize /
    dequantize round trip costs a relative max error of ~3.4% per weight tensor.

    Returns the number of bytes saved, and that figure is now TRUE. The encoder
    is loaded as `Qwen3Model`, which has no `lm_head` at all; under the previous
    `Qwen3ForCausalLM` the walk also hit that layer, whose weight is TIED to
    `model.embed_tokens` (verified: identical `data_ptr()`). Quantizing it freed
    nothing — the bf16 storage stayed alive through the embedding — while ADDING
    the fp8 copy and its scales, and counted the whole tied tensor as saved. The
    class change removes the case; do not add tied-weight special-casing to
    bring it back.

    **It does not lower the load peak**: the bf16 weights must exist before they
    can be quantized, so it pays off only while the encoder stays resident
    (`unload_text_encoder_after_encode=False`).

    # Raises
    `RuntimeError` when the running torch build has no `float8_e4m3fn`. The flag
    is never silently ignored: the user would be billed for a quality trade they
    did not actually get.
    """
    import torch

    fp8_dtype = getattr(torch, "float8_e4m3fn", None)
    if fp8_dtype is None:
        raise RuntimeError(
            f"fp8 для текстового энкодера не поддерживается этой сборкой torch "
            f"({torch.__version__}): нет типа float8_e4m3fn. Отключите параметр "
            "«fp8 для текстового энкодера»."
        )

    class _Fp8Linear(torch.nn.Module):
        """`nn.Linear` whose weight is stored as float8 with per-output-row scales.

        The class is built here rather than at module level because this module
        must import without torch. `bytes_saved` reports how much one swap freed;
        the bias stays in the original dtype, being tiny and the most numerically
        sensitive part of the layer.
        """

        def __init__(self, source: torch.nn.Linear) -> None:
            super().__init__()
            weight = source.weight.data
            self.compute_dtype = weight.dtype
            # 448 is the largest finite magnitude of float8_e4m3fn; scaling each
            # output row to that maximum keeps the row's full dynamic range.
            scale = weight.abs().amax(dim=1, keepdim=True).clamp(min=1e-6) / 448.0
            self.register_buffer("weight_fp8", (weight / scale).to(fp8_dtype))
            self.register_buffer("weight_scale", scale.to(weight.dtype))
            self.bias = source.bias
            self.bytes_saved = weight.numel() * weight.element_size() - (
                self.weight_fp8.numel() * self.weight_fp8.element_size()
                + self.weight_scale.numel() * self.weight_scale.element_size()
            )

        def forward(self, x: torch.Tensor) -> torch.Tensor:
            weight = self.weight_fp8.to(self.compute_dtype) * self.weight_scale
            return torch.nn.functional.linear(x, weight, self.bias)

    saved = 0
    swapped = 0
    for parent in list(text_encoder.modules()):
        for name, child in list(parent.named_children()):
            if not isinstance(child, torch.nn.Linear):
                continue
            replacement = _Fp8Linear(child)
            saved += replacement.bytes_saved
            # Swapping in place drops the last reference to the bf16 weight, so
            # the peak stays one layer above the original size, not twice it.
            setattr(parent, name, replacement)
            swapped += 1
    log.info(
        "FLUX.2 klein: текстовый энкодер квантован в fp8 (%d линейных слоёв, освобождено %.2f ГиБ). "
        "На ПИК памяти это не влияет — экономия видна только пока энкодер остаётся резидентным.",
        swapped,
        saved / (1024**3),
    )
    return saved


def _require_execution_device(pipe: Any, device: Any) -> None:
    """Check the pipeline will build its tensors on `device`, or raise saying why.

    `_execution_device` is what `__call__` uses to place the region image, the
    noise and the mask latents, and it is derived from the components rather
    than passed in: with no accelerate hooks it degrades to the device of the
    first component in sorted signature order. A component left on the host
    therefore turns into a `conv2d` "Input type (CPUBFloat16Type) and weight
    type (CUDABFloat16Type)" several frames deep inside the VAE, naming neither
    the component nor the placement. This turns the same condition into one
    sentence the user can act on, and is a no-op whenever placement did its job.
    A pipeline without the property (a test double) is not probed.

    # Raises
    `RuntimeError` naming every component's device when the probe disagrees with
    `device`.
    """
    probed = getattr(pipe, "_execution_device", None)
    if probed is None or str(probed) == str(device):
        return
    devices = ", ".join(
        f"{name}={getattr(getattr(pipe, name, None), 'device', 'нет')}"
        for name in ("transformer", "vae", "text_encoder")
    )
    raise RuntimeError(
        f"FLUX.2 klein: пайплайн собирается считать на «{probed}», хотя трансформер размещён на "
        f"«{device}». Размещение компонентов: {devices}. Запуск остановлен до обращения к модели — "
        "иначе ошибка всплыла бы внутри VAE как несовпадение устройств."
    )


# =====================================================================
#  VAE decode: transformer parking and OOM recovery
# =====================================================================
def _is_out_of_memory(exc: BaseException) -> bool:
    """Whether `exc` is an accelerator out-of-memory failure.

    Two shapes have to be recognised: `torch.OutOfMemoryError` (and its
    `torch.cuda` alias) on recent builds, and a plain `RuntimeError` whose text
    contains "out of memory", which is what ROCm/HIP and older builds raise.
    """
    try:
        import torch
    except ImportError:  # pragma: no cover - torch is present whenever we decode
        return isinstance(exc, RuntimeError) and "out of memory" in str(exc).lower()

    for holder in (torch, getattr(torch, "cuda", None)):
        oom_type = getattr(holder, "OutOfMemoryError", None)
        if isinstance(oom_type, type) and isinstance(exc, oom_type):
            return True
    return isinstance(exc, RuntimeError) and "out of memory" in str(exc).lower()


def _park_transformer_off_device(pipe: Any, placement: str) -> bool:
    """Free the transformer's device memory before the VAE decode.

    Returns whether the module was actually moved, i.e. whether the caller owes
    it a move back. Under the two accelerate offload placements it never is:
    the hooks already returned the transformer to host memory after the last
    forward, so all that is left to do is release the allocator's blocks.
    """
    hardware._clear_torch_cache()
    if placement in ("model_cpu_offload", "sequential_cpu_offload"):
        return False
    transformer = getattr(pipe, "transformer", None)
    if transformer is None:
        return False
    if getattr(getattr(transformer, "device", None), "type", "cpu") == "cpu":
        return False
    transformer.to("cpu")
    hardware._clear_torch_cache()
    return True


def _restore_transformer_to_device(pipe: Any, device: Any) -> None:
    """Move a parked transformer back so the cached pipeline stays usable.

    Dropping it instead would force a multi-second reload from disk on the next
    request and leave `_active_key` describing a pipeline that no longer exists;
    the move back is a plain host->device copy out of anonymous memory.

    # Raises
    Whatever the copy raises — at 9B it can itself run out of memory. The caller
    owns that case: see `Flux2KleinInpaintService._decode_locked`.
    """
    transformer = getattr(pipe, "transformer", None)
    if transformer is None or device is None:
        return
    with patched_module_to():
        transformer.to(device)


def _decode_once(pipe: Any, latents_cpu: Any) -> Any:
    """Decode a CPU copy of the latents into one PIL image.

    The latents go to `_vae_input_device`, which is the VAE's own device except
    under accelerate's sequential offload, where the parameters live on `meta`
    between forwards.

    `torch.no_grad()` is explicit here because this decode is deliberately NOT
    inside `pipeline.__call__`, which carries the decorator: without it the VAE
    builds an autograd graph over a full-resolution image and
    `image_processor.postprocess` dies on `Can't call numpy() on Tensor that
    requires grad`.
    """
    import torch

    with torch.no_grad():
        latents = latents_cpu.to(device=_vae_input_device(pipe), dtype=pipe.vae.dtype)
        image = pipe.vae.decode(latents, return_dict=False)[0]
        return pipe.image_processor.postprocess(image, output_type="pil")[0]


def _vae_input_device(pipe: Any) -> Any:
    """Device the VAE's `decode` input must be on.

    Two different answers, and picking the wrong one is a device mismatch several
    frames inside torch:

    - **No accelerate hook** (`full_gpu`, `encoder_cpu`): the VAE's OWN device.
      Not the pipeline's execution device — by decode time the transformer may
      have been parked on the host, which is exactly what
      `unload_transformer_before_vae` does, and `_execution_device` would then
      answer `cpu` while the VAE sits on the accelerator.
    - **Under an accelerate offload hook** (`model_cpu_offload`,
      `sequential_cpu_offload`): the pipeline's execution device. `vae.device` is
      `cpu` or `meta` there, and diffusers' `@apply_forward_hook` calls
      `pre_forward(self)` WITHOUT the arguments, so the hook moves the WEIGHTS to
      the accelerator and leaves our latents behind — a CPU input against CUDA
      weights, or "Cannot copy out of meta tensor" if we followed `meta`.
    """
    vae = getattr(pipe, "vae", None)
    device = getattr(vae, "device", None)
    hooked = hasattr(vae, "_hf_hook")
    if not hooked and device is not None and getattr(device, "type", None) != "meta":
        return device
    return pipe._execution_device


def _latent_spatial_sides(latents: Any) -> tuple[int, int] | None:
    """`(height, width)` of a latent tensor in latent cells, or `None`.

    The two trailing dimensions are exactly the ones `AutoencoderKLFlux2._decode`
    compares against `tile_latent_min_size`, so reading them here keeps the two
    gates talking about the same numbers. `None` whenever the object does not
    expose a usable 2-D-or-deeper `shape` — a stand-in, or a future tensor type —
    and every caller then declines to CLAIM anything rather than guessing.
    """
    shape = getattr(latents, "shape", None)
    if shape is None:
        return None
    try:
        dims = tuple(int(dim) for dim in shape)
    except (TypeError, ValueError):
        return None
    if len(dims) < 2 or dims[-1] <= 0 or dims[-2] <= 0:
        return None
    return dims[-2], dims[-1]


def _vae_tile_latent_min_size(vae: Any) -> int | None:
    """The VAE's own latent tiling threshold, or `None` when it has none.

    Read from the instance rather than recomputed from the config: the
    thresholds are mutable attributes and the last rung of the OOM ladder lowers
    them, so the instance is the only place that knows the current value.
    """
    value = getattr(vae, "tile_latent_min_size", None)
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        return None
    return value


def _vae_latent_scale_factor(vae: Any) -> int | None:
    """Pixels per latent cell for this VAE, or `None` when its config is unusable.

    `2 ** (len(block_out_channels) - 1)`, which is the very factor
    `AutoencoderKLFlux2.__init__` uses to derive `tile_latent_min_size` from
    `tile_sample_min_size`. `None` means the paired thresholds cannot be computed
    and the last rung of the OOM ladder must not run: changing one of the two
    alone is a corrupt image, not a saving (see `_lowered_vae_tile_thresholds`).
    """
    blocks = getattr(getattr(vae, "config", None), "block_out_channels", None)
    if not isinstance(blocks, (list, tuple)) or not blocks:
        return None
    return 2 ** (len(blocks) - 1)


def _vae_tiling_engages(pipe: Any, latents_cpu: Any) -> bool:
    """Whether a tiled decode would ACTUALLY happen for these latents.

    Mirrors diffusers' own gate (`AutoencoderKLFlux2._decode`: the tiled branch is
    taken when a latent side exceeds `tile_latent_min_size`) instead of repeating
    the 1024/128 numbers, so an upstream or per-VAE change of the threshold moves
    both together. `False` whenever the answer cannot be established — a VAE
    without the attribute, latents without a shape — because every caller uses
    this to decide whether to CLAIM a saving, and an unprovable claim is a lie
    written into the user's settings file.
    """
    sides = _latent_spatial_sides(latents_cpu)
    threshold = _vae_tile_latent_min_size(getattr(pipe, "vae", None))
    if sides is None or threshold is None:
        return False
    return max(sides) > threshold


@contextlib.contextmanager
def _lowered_vae_tile_thresholds(vae: Any, latent_min_size: int, scale: int) -> Iterator[None]:
    """Temporarily shrink the VAE's tile, restoring both thresholds afterwards.

    **The two thresholds are ONE setting and must move together.**
    `tiled_decode` slices the latents by `tile_latent_min_size` but blends and
    crops the decoded tiles by `tile_sample_min_size`
    (`overlap_size`/`blend_extent`/`row_limit` in
    `diffusers/models/autoencoders/autoencoder_kl_flux2.py`), and its arithmetic
    assumes `tile_sample_min_size == tile_latent_min_size * scale`. A sibling
    project lowered only the latent side and got a 1024x1024 image rebuilt as a
    mosaic of nine shifted copies of the scene with visible seams. `scale` is
    therefore required, and `_vae_latent_scale_factor` refuses the whole rung
    when it cannot be computed.

    The change is PER DECODE: the thresholds are restored in a `finally` so that
    nothing leaks into the next request, the warm-up, or a cached pipeline
    another request will hit.
    """
    missing = object()
    previous_latent = vae.tile_latent_min_size
    # A VAE that never declared the sample-side threshold must not come back
    # carrying one: restoring an absent attribute as `None` would leave
    # `tiled_decode` reading it as a number on the next request.
    previous_sample = getattr(vae, "tile_sample_min_size", missing)
    vae.tile_latent_min_size = latent_min_size
    vae.tile_sample_min_size = latent_min_size * scale
    try:
        yield
    finally:
        vae.tile_latent_min_size = previous_latent
        if previous_sample is missing:
            delattr(vae, "tile_sample_min_size")
        else:
            vae.tile_sample_min_size = previous_sample


def _emergency_tile_latent_size(pipe: Any, latents_cpu: Any) -> tuple[int, int] | None:
    """`(latent tile size, scale)` for the last rung, or `None` when it cannot run.

    The rung is worth running only when every part of it holds: the VAE exposes
    a threshold and a usable config, the threshold is genuinely ABOVE
    `EMERGENCY_TILE_LATENT_MIN_SIZE` (otherwise there is nothing to lower), and
    the latents are larger than the emergency tile (otherwise the gate still will
    not open and the decode would be repeated for nothing). A region smaller than
    one emergency tile is refused rather than tiled finer: its decode is already
    the cheapest this VAE can do, and the shortfall is elsewhere.
    """
    vae = getattr(pipe, "vae", None)
    threshold = _vae_tile_latent_min_size(vae)
    scale = _vae_latent_scale_factor(vae)
    sides = _latent_spatial_sides(latents_cpu)
    if threshold is None or scale is None or sides is None:
        return None
    target = EMERGENCY_TILE_LATENT_MIN_SIZE
    if threshold <= target or max(sides) <= target:
        return None
    return target, scale


def _tiling_gate_note(pipe: Any, latents_cpu: Any) -> str:
    """Sentence for the final refusal when tiling could not engage, else `""`.

    §7 wants a refusal that says what to do next, and "включите тайлинг" is not
    it when the flag is already on and the region is simply below the threshold.
    """
    if _vae_tiling_engages(pipe, latents_cpu):
        return ""
    vae = getattr(pipe, "vae", None)
    threshold = _vae_tile_latent_min_size(vae)
    sides = _latent_spatial_sides(latents_cpu)
    if threshold is None or sides is None:
        return ""
    scale = _vae_latent_scale_factor(vae)
    if scale is None:
        return (
            f"Тайлинг VAE на этой области не включается: он начинает действовать только когда "
            f"сторона латентов превышает {threshold} ячеек, а здесь это "
            f"{sides[1]}x{sides[0]}. "
        )
    return (
        f"Тайлинг VAE на этой области не включается: он начинает действовать только когда "
        f"сторона области превышает {threshold * scale} px, а здесь это "
        f"{sides[1] * scale}x{sides[0] * scale} px. "
    )


def _decode_region_latents(
    pipe: Any,
    latents_cpu: Any,
    normalized: dict[str, Any],
    park_transformer: Callable[[], bool],
) -> tuple[Any, dict[str, bool], bool]:
    """Decode the latents, escalating memory savings on an out-of-memory failure.

    The denoise is never repeated: every attempt starts from the same host copy
    of the latents. Escalation order:

    1. park the transformer on the host;
    2. enable VAE tiling — but ONLY when it can actually engage for these
       latents (`_vae_tiling_engages`). Below the VAE's threshold the flag
       changes nothing, so setting `applied["vae_tiling"]` there would write a
       saving that never happened into the user's settings file;
    3. shrink the VAE's tile (`_lowered_vae_tile_thresholds`) so that tiling
       engages on a region the shipped threshold ignores. This is the only rung
       that helps the common case, since no square region this service accepts
       reaches 1024 px.

    `vae_slicing` is NOT a rung: `decode` slices only when `z.shape[0] > 1` and
    this service always decodes a batch of one, so enabling it can never save a
    byte here (`dev-docs/known_gaps.md`, KG-008).

    Rung 3 is an emergency, not a setting: the thresholds are restored before
    returning and never appear in `applied`, which carries the five persisted
    memory flags and nothing else. Its result is also not bit-identical to an
    untiled decode — `tiled_decode` blends overlapping tiles — which is
    acceptable precisely because the answer carries `oom_recovered`, telling the
    caller a retry happened.

    Returns `(image, applied, oom_recovered)`.

    # Raises
    `RuntimeError` naming what was tried and the free-memory figures when even
    the last attempt runs out of memory, and re-raises unchanged anything that is
    not an OOM.
    """
    applied = {
        "unload_transformer_before_vae": bool(normalized["unload_transformer_before_vae"]),
        "vae_tiling": bool(normalized["vae_tiling"]),
        "vae_slicing": bool(normalized["vae_slicing"]),
        # Not touched by the OOM ladder, but part of the same "what actually ran"
        # answer the Rust side persists back into the settings.
        "unload_text_encoder_after_encode": bool(normalized["unload_text_encoder_after_encode"]),
        "text_encoder_fp8": bool(normalized["text_encoder_fp8"]),
    }
    try:
        return _decode_once(pipe, latents_cpu), applied, False
    except Exception as exc:  # noqa: BLE001 - re-raised below unless it is an OOM
        if not _is_out_of_memory(exc):
            raise
        last_error: BaseException = exc
        before = hardware.memory_snapshot()
        log.warning(
            "FLUX.2 klein: VAE decode ran out of memory (%s). Free VRAM %d B of %d B; "
            "recovering without repeating the denoise (settings were "
            "unload_transformer_before_vae=%s, vae_tiling=%s, vae_slicing=%s).",
            exc,
            before["vram_free"],
            before["vram_total"],
            applied["unload_transformer_before_vae"],
            applied["vae_tiling"],
            applied["vae_slicing"],
        )

    #: Whether a rung actually decoded with tiling in force. Not the same as
    #: `applied["vae_tiling"]`, which can be `True` from the user's settings on a
    #: region where tiling never engages — and the final refusal must not claim
    #: to have tried something that could not happen.
    tiling_attempted = _vae_tiling_engages(pipe, latents_cpu) and applied["vae_tiling"]

    # Step 1: get the transformer out of the way and retry.
    if not applied["unload_transformer_before_vae"]:
        park_transformer()
        applied["unload_transformer_before_vae"] = True
        after = hardware.memory_snapshot()
        log.warning(
            "FLUX.2 klein: retrying the VAE decode with the transformer parked on the host "
            "(free VRAM %d B).",
            after["vram_free"],
        )
        try:
            return _decode_once(pipe, latents_cpu), applied, True
        except Exception as exc:  # noqa: BLE001 - checked immediately below
            if not _is_out_of_memory(exc):
                raise
            last_error = exc

    # Step 2: cut the decode's own peak with tiling — but only where the tiled
    # path would really be entered. Claiming it on a region below the VAE's
    # threshold costs a repeated decode and writes a false saving back into the
    # user's settings, which is what the memory guard then plans against.
    if not applied["vae_tiling"] and _vae_tiling_engages(pipe, latents_cpu):
        applied["vae_tiling"] = True
        tiling_attempted = True
        _apply_vae_memory_options(pipe, applied)
        after = hardware.memory_snapshot()
        log.warning(
            "FLUX.2 klein: retrying the VAE decode with tiling enabled (free VRAM %d B).",
            after["vram_free"],
        )
        try:
            return _decode_once(pipe, latents_cpu), applied, True
        except Exception as exc:  # noqa: BLE001 - checked immediately below
            if not _is_out_of_memory(exc):
                raise
            last_error = exc

    # Step 3: make tiling possible at all by shrinking the tile itself. The
    # thresholds belong to the VAE object, so the change is scoped to this decode
    # and undone before the next request can see it.
    emergency = _emergency_tile_latent_size(pipe, latents_cpu)
    if emergency is not None:
        target, scale = emergency
        with _lowered_vae_tile_thresholds(pipe.vae, target, scale):
            # Tiling has to be ON for the lowered threshold to be consulted at
            # all; it genuinely runs tiled here, so `applied` may say so.
            applied["vae_tiling"] = True
            tiling_attempted = True
            _apply_vae_memory_options(pipe, applied)
            after = hardware.memory_snapshot()
            log.warning(
                "FLUX.2 klein: retrying the VAE decode with the VAE tile lowered to %d latent "
                "cells / %d px (free VRAM %d B). The tiles are blended, so the image is not "
                "bit-identical to an untiled decode.",
                target,
                target * scale,
                after["vram_free"],
            )
            try:
                return _decode_once(pipe, latents_cpu), applied, True
            except Exception as exc:  # noqa: BLE001 - checked immediately below
                if not _is_out_of_memory(exc):
                    raise
                last_error = exc

    final = hardware.memory_snapshot()
    tried = ["выгрузка трансформера"]
    if tiling_attempted:
        tried.append("тайлинг VAE")
    if emergency is not None:
        tried.append(f"уменьшение тайла VAE до {emergency[0] * emergency[1]} px")
    raise RuntimeError(
        f"Не хватило видеопамяти на декодирование VAE. Испробовано: {', '.join(tried)}. "
        f"Свободно {final['vram_free']} байт из {final['vram_total']}. "
        f"{_tiling_gate_note(pipe, latents_cpu)}"
        "Уменьшите выделенную область или выберите режим размещения с меньшим расходом "
        f"видеопамяти. Исходная ошибка: {last_error}"
    ) from last_error


# =====================================================================
#  ROCm staging for the offload placements
# =====================================================================
def _largest_cpu_tensor(module: Any) -> Any:
    """Largest CPU-resident parameter or buffer of `module`, or `None`.

    Used as the probe for `_component_is_file_backed`. Only CPU tensors qualify:
    a component that already sits on the GPU has nothing left to re-home.
    """
    largest = None
    largest_bytes = -1
    for tensor in list(module.parameters()) + list(module.buffers()):
        if getattr(getattr(tensor, "device", None), "type", None) != "cpu":
            continue
        nbytes = int(tensor.numel()) * int(tensor.element_size())
        if nbytes > largest_bytes:
            largest = tensor
            largest_bytes = nbytes
    return largest


def _component_is_file_backed(module: Any) -> bool:
    """Whether `module`'s weights still live in the safetensors file mapping.

    Only such weights hit the amdkfd stall, so only they are worth the round
    trip. A component is loaded in one pass from its own file, so its largest
    CPU tensor is representative of all of them.
    """
    probe = _largest_cpu_tensor(module)
    if probe is None:
        return False
    return tensor_needs_staging(probe)


def _materialize_components_for_offload(pipe: Any, device: Any) -> None:
    """Re-home the safetensors components in anonymous host memory before offload.

    Accelerate's offload hooks move a component to the GPU lazily from inside the
    forward pass. On ROCm that first lazy move copies straight out of the
    safetensors mapping and stalls in amdkfd (~1-2 s per tensor of >=1 MiB), and
    `patched_module_to` cannot be held there — it is process-global and its
    contract forbids wrapping inference. So each component makes one staged
    round trip now, which leaves its resident CPU copy in freshly allocated
    anonymous memory.

    Skipped entirely off ROCm, and per component when its weights are no longer
    file-backed. A failed move is a lost optimization, not a lost model: it is
    logged and the load continues. The transformer is attempted last because it
    is the one component whose round trip can plausibly exhaust VRAM.

    This mirrors `flux_fill.py`'s helper; the two are deliberate copies rather
    than a shared import, because `inpaint/MODULE_README.md` forbids a service in
    this package from importing a sibling service.
    """
    if not mmap_staging_required():
        return

    import torch

    started_at = time.perf_counter()
    rehomed: list[str] = []
    skipped: list[str] = []
    for name in _MMAP_BACKED_COMPONENTS:
        module = getattr(pipe, name, None)
        if not isinstance(module, torch.nn.Module):
            continue
        if not _component_is_file_backed(module):
            skipped.append(name)
            continue
        try:
            with patched_module_to():
                module.to(device)
            # The way back allocates fresh anonymous host memory for every
            # tensor — that copy is the whole point of the round trip.
            module.to("cpu")
        except (RuntimeError, MemoryError) as exc:
            log.warning(
                "FLUX.2 klein: could not re-home component %r in anonymous host memory before "
                "CPU offload (%s). The model still works, but on ROCm the first generation may "
                "stall in the mmap->GPU weight copy.",
                name,
                exc,
            )
            break
        # Release this component's device blocks before the next, larger one is
        # staged: the allocator would otherwise reserve on top of them.
        hardware._clear_torch_cache()
        rehomed.append(name)

    hardware._clear_torch_cache()
    if rehomed:
        log.info(
            "FLUX.2 klein: re-homed %s in anonymous host memory in %.2f s before enabling CPU "
            "offload (ROCm mmap->GPU stall workaround).",
            ", ".join(rehomed),
            time.perf_counter() - started_at,
        )
    if skipped:
        log.debug(
            "FLUX.2 klein: %s already live in anonymous host memory; skipped the round trip.",
            ", ".join(skipped),
        )
