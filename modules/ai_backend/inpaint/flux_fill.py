"""
File: modules/ai_backend/inpaint/flux_fill.py

Purpose:
FLUX.1-Fill-dev inpainting service for the Python AI backend (method
`inpaint.flux_fill`, streaming). Powers the "AI редактирование области" cleaning
tool: regenerates / removes content under a painted mask.

Model layout (everything under `ManhwaStudio_AI_Models/side_models/`, NOT the HF
cache):
- transformer: a GGUF quant chosen by the user, downloaded on demand from
  `YarvixPA/FLUX.1-Fill-dev-GGUF` into `side_models/FLUX.1-Fill-dev-GGUF`;
- VAE / CLIP-L / T5-XXL / scheduler / tokenizers: diffusers components downloaded
  from the open (non-gated) `ostris/Flex.1-alpha` repo (architecturally identical
  to FLUX.1) into its `components/` subdirectory. Both directories derive from
  `runtime.paths.side_models_root()`, the backend's single owner of that root.

Main responsibilities:
- list/quant management + on-demand download with byte-level progress, sized
  from the hub listing and retried / resumed / cancelled through
  `engines/model_download.download_bearer_to_path`; the resolved listing is
  cached per quant (`flux1-fill-dev-<quant>.listing.json`), so a complete model
  needs no network;
- lazy pipeline build (GGUF transformer + local components), pinned to the
  DISCRETE GPU (the integrated Ryzen iGPU is explicitly excluded);
- MIOpen immediate mode on ROCm (set before torch import);
- generation with mask dilation, seamless (Poisson) tone matching, and compositing
  back over the original outside the mask;
- progress streamed as `progress_callback(phase, step, total, label)` where phase
  is "download" or "generate";
- health / unload hooks; reuse of the shared resident-model manager.

Notes:
The heavy packages (torch/diffusers/transformers) are imported lazily. Missing
packages, weights, or download failures surface as explicit errors.
At the default `dtype: "bf16"` the safetensors components (VAE / CLIP / T5) are
requested at the dtype they are already stored in, so no host-side cast
materializes them in anonymous memory and every host->device copy of theirs hits
the ROCm amdkfd stall. Both placement paths therefore go through
`runtime/rocm_mmap_transfer.py`: the plain path wraps `pipe.to(device)` in
`patched_module_to()`, the CPU-offload path re-homes the components up front
(see `_materialize_components_for_offload`). Both are strict no-ops off ROCm.
Neither the GGUF transformer nor a `dtype: "fp16"` run is affected: the GGUF
reader and the fp16 host-side cast both already copy into anonymous memory, and
the per-tensor file-backing checks keep those cases out of the workaround.
"""

from __future__ import annotations

import functools
import io
import json
import logging
import os
import threading
import time
from typing import TYPE_CHECKING, Any, Callable

if TYPE_CHECKING:
    import numpy as np

from ..engines.model_download import (
    RetryEvent,
    download_bearer_to_path,
    hub_lfs_sha256,
    retry_progress_label,
)
from ..runtime.health_snapshot import HealthSnapshot
from ..runtime.model_manager import SmallModelManager
from ..runtime.paths import side_models_root
from ..runtime.residency.footprint import hint_from_files
from ..runtime.rocm_mmap_transfer import (
    mmap_staging_required,
    patched_module_to,
    tensor_needs_staging,
)
from ..runtime.torch_device import clear_torch_cache, resolve_discrete_backend_device

log = logging.getLogger(__name__)

# --- Repos / quants -------------------------------------------------------
GGUF_REPO = "YarvixPA/FLUX.1-Fill-dev-GGUF"
COMPONENTS_REPO = "ostris/Flex.1-alpha"
# Component subfolders to pull (the transformer folder is intentionally skipped —
# we use the local GGUF transformer instead).
COMPONENT_PREFIXES = (
    "model_index.json",
    "scheduler/",
    "vae/",
    "text_encoder/",
    "text_encoder_2/",
    "tokenizer/",
    "tokenizer_2/",
)

# Available GGUF quants in GGUF_REPO (filename = flux1-fill-dev-{QUANT}.gguf),
# ordered small -> large. Q8_0 is the default (best quality, ~12.6 GB).
AVAILABLE_QUANTS = (
    "Q3_K_S",
    "Q4_0",
    "Q4_1",
    "Q4_K_S",
    "Q5_0",
    "Q5_1",
    "Q5_K_S",
    "Q6_K",
    "Q8_0",
)
DEFAULT_QUANT = "Q8_0"
#: Owner name of this service's leases in the residency arbiter (owner rule).
_LEASE_OWNER = "inpaint.flux_fill"

OBJECT_REMOVAL_PROMPT = (
    "clean background, seamless continuation of the surrounding texture, "
    "consistent lighting and color"
)
VALID_MODES = ("inpaint", "object_removal")

# Progress callback: (phase, step, total, label). phase in {"download","generate"}.
ProgressCb = Callable[[str, int, int, str], None]


def gguf_filename(quant: str) -> str:
    return f"flux1-fill-dev-{quant}.gguf"


def gguf_path(quant: str) -> str:
    return os.path.join(_flux_dir(), gguf_filename(quant))


def _flux_dir() -> str:
    """`<side_models>/FLUX.1-Fill-dev-GGUF`: the GGUF transformer quants.

    Returned as `str` because callers do string path arithmetic on it
    (`os.path.join`, `os.path.dirname`).
    """
    return os.fspath(side_models_root() / "FLUX.1-Fill-dev-GGUF")


def _components_dir() -> str:
    """`<flux dir>/components`: the diffusers VAE / CLIP / T5 / scheduler components."""
    return os.path.join(_flux_dir(), "components")


def normalize_quant(value: Any) -> str:
    q = str(value or "").strip()
    return q if q in AVAILABLE_QUANTS else DEFAULT_QUANT


# =====================================================================
#  Parameter normalization
# =====================================================================
def normalize_flux_fill_params(params: dict[str, Any] | None) -> dict[str, Any]:
    merged: dict[str, Any] = {}
    if isinstance(params, dict):
        merged.update(params)

    mode = str(merged.get("mode", "object_removal") or "").strip()
    if mode not in VALID_MODES:
        raise ValueError(f"Неизвестный режим Flux Fill: {mode!r}")

    quant = normalize_quant(merged.get("quant"))

    prompt = str(merged.get("prompt", "") or "").strip()
    if mode == "object_removal" and not prompt:
        prompt = OBJECT_REMOVAL_PROMPT

    return {
        "mode": mode,
        "quant": quant,
        "prompt": prompt,
        "steps": _clamp_int(merged.get("steps"), default=28, low=1, high=100),
        "guidance": _clamp_float(merged.get("guidance"), default=30.0, low=0.0, high=100.0),
        "seed": _to_int(merged.get("seed"), -1),
        "max_seq": _clamp_int(merged.get("max_seq"), default=512, low=64, high=512),
        "max_side": _clamp_int(merged.get("max_side"), default=1536, low=0, high=4096),
        "dilate": _clamp_int(merged.get("dilate"), default=4, low=0, high=100),
        "feather": _clamp_int(merged.get("feather"), default=3, low=0, high=100),
        "seamless": _to_bool(merged.get("seamless"), True),
        "vae_tiling": _to_bool(merged.get("vae_tiling"), True),
        "cpu_offload": _to_bool(merged.get("cpu_offload"), False),
        "miopen_fast": _to_bool(merged.get("miopen_fast"), True),
        "dtype": "fp16" if str(merged.get("dtype", "bf16")).strip() == "fp16" else "bf16",
    }


# =====================================================================
#  Service
# =====================================================================
class FluxFillInpaintService:
    """Lazy-loading FLUX.1-Fill-dev inpaint pipeline for `inpaint.flux_fill`."""

    def __init__(self, model_manager: SmallModelManager) -> None:
        self._lock = threading.RLock()
        self._model_manager = model_manager
        self._pipe: Any = None
        self._active_key: str | None = None
        self._device: Any = None
        self._last_error: str | None = None
        # Published copy of the state-derived health fields; `health()` never
        # takes `_lock`, which a pipeline load and generation hold (S-014).
        self._health = HealthSnapshot()
        self._publish_health_locked()

    # ---- status / health ----
    def status(self) -> dict[str, Any]:
        """Quant catalog + which quants and components are already on disk."""
        downloaded = [q for q in AVAILABLE_QUANTS if _is_nonempty_file(gguf_path(q))]
        return {
            "quants": list(AVAILABLE_QUANTS),
            "default_quant": DEFAULT_QUANT,
            "downloaded_quants": downloaded,
            "components_ready": _components_present(),
            "gguf_repo": GGUF_REPO,
            "components_repo": COMPONENTS_REPO,
        }

    def health(self) -> dict[str, Any]:
        """Return the FLUX.1-Fill health entry for the backend snapshot.

        State fields come from the published snapshot; the on-disk quant and
        component probes run here, outside any service lock, so this answers
        while a pipeline load or a generation holds the instance lock.
        """
        fields = self._health.read()
        return {
            "ready": fields["ready"],
            "model": "flux_fill",
            "device": fields["device"],
            "active_key": fields["active_key"],
            "downloaded_quants": [q for q in AVAILABLE_QUANTS if _is_nonempty_file(gguf_path(q))],
            "components_ready": _components_present(),
            "last_error": fields["last_error"],
        }

    def _publish_health_locked(self) -> None:
        """Republish the state-derived health fields; caller holds `self._lock` (or is `__init__`)."""
        self._health.publish(
            ready=self._pipe is not None,
            device=str(self._device) if self._device is not None else "cpu",
            active_key=self._active_key,
            last_error=self._last_error,
        )

    def unload(self) -> bool:
        """Drop the resident pipeline; `False` when nothing was loaded."""
        with self._lock:
            if self._pipe is None:
                return False
            key = self._active_key
            self._pipe = None
            self._active_key = None
            self._publish_health_locked()
            _clear_torch_cache()
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
        should_cancel: Callable[[], bool] | None = None,
    ) -> dict[str, Any]:
        """Inpaint `image_bytes` under `mask_bytes`, downloading the weights first if needed.

        `should_cancel` is polled during the weight download (every chunk and
        every backoff slice); a cancel there raises
        `engines.model_download.DownloadCancelled`. Generation itself is not
        interruptible here.
        """
        normalized = normalize_flux_fill_params(params)
        image_rgb = _decode_image_rgb(image_bytes)
        mask_u8 = _decode_mask(mask_bytes, expected_hw=image_rgb.shape[:2])

        # 1) Make sure the weights are on disk (streams download progress).
        self.ensure_model(normalized["quant"], progress_callback, should_cancel)

        quant = normalized["quant"]
        model_key = f"flux_fill:{quant}"
        # Pre-load size: the GGUF transformer plus every diffusers component
        # `_ensure_pipeline_locked` loads. The device is the one that load
        # will pick (a read-only probe, made current only by the load); with
        # CPU offload the weights stay in RAM, so they are host-accounted.
        hint_device = "cpu" if normalized["cpu_offload"] else resolve_discrete_backend_device("cuda", honour_configured=False)
        hint = hint_from_files([gguf_path(quant), _components_dir()], hint_device)
        lease = self._model_manager.begin_model_use(
            model_key,
            unload_callback=lambda: self._unload_key(model_key),
            owner=_LEASE_OWNER,
            hint=hint,
        )
        with self._lock:
            try:
                # Load scope: only a failure in here is a failed LOAD, and only
                # then may the manager drop its entry for `model_key`.
                try:
                    pipe = self._ensure_pipeline_locked(normalized, model_key)
                except Exception:
                    if lease.needs_load:
                        lease.mark_load_failed()
                    raise
                # The pipeline is resident from here on, so it is registered
                # before generation: a generation failure must leave it counted
                # and evictable, not silently occupying VRAM off the books.
                lease.mark_loaded(unload_callback=lambda: self._unload_key(model_key))
                out_rgb = self._generate_locked(
                    pipe, image_rgb, mask_u8, normalized, progress_callback
                )
                self._last_error = None
                self._publish_health_locked()
            except Exception as exc:
                self._last_error = str(exc)
                self._publish_health_locked()
                raise
            finally:
                lease.release()

        return {
            "image_png": _encode_png_bytes_rgb(out_rgb),
            "source_size": [int(image_rgb.shape[1]), int(image_rgb.shape[0])],
            "device": str(self._device) if self._device is not None else "cpu",
            "mode": normalized["mode"],
            "quant": quant,
        }

    # ---- model download ----
    def ensure_model(
        self,
        quant: str,
        progress_callback: ProgressCb | None,
        should_cancel: Callable[[], bool] | None = None,
    ) -> None:
        """Download the selected GGUF quant + diffusers components into side_models.

        Files already present at the size the hub listing announced are skipped.
        The listing is fetched only when a file is missing: the last resolved
        plan is cached next to the model (`_load_cached_plan`), so a complete
        model runs offline. Without a cache (a model downloaded by an older
        build) a failed listing is tolerated when the quant and the components
        are already on disk — every published file passed its size / sha256
        gate — and logged; otherwise it raises. Progress is reported in bytes via `progress_callback("download", done,
        total, label)`; a transient network failure is retried by the shared
        helper and shown as a retry `label` on the same frame shape, not as an
        error. `should_cancel` is polled per chunk and during every backoff wait.

        Runs OUTSIDE `self._lock` on purpose — a multi-GB download must never
        block `health()`, `unload()` or an eviction callback — so two concurrent
        first uses reach this method at once. Each individual file is serialized
        and staged privately by `_download_file_streaming`; a file the other
        thread already finished is skipped here rather than fetched twice.
        """
        quant = normalize_quant(quant)
        os.makedirs(_flux_dir(), exist_ok=True)
        os.makedirs(_components_dir(), exist_ok=True)

        cached = _load_cached_plan(quant)
        if cached is not None and all(_is_complete_file(item["dest"], int(item["size"])) for item in cached):
            return
        try:
            plan = _build_download_plan(quant)
        except RuntimeError:
            if cached is None and _is_nonempty_file(gguf_path(quant)) and _components_present():
                log.warning(
                    "FLUX Fill: listing failed and no cached plan exists for %s; running the "
                    "model already on disk without re-checking it against the hub",
                    quant,
                    exc_info=True,
                )
                return
            raise
        _save_cached_plan(quant, plan)
        # Filter to missing files only.
        missing = [item for item in plan if not _is_complete_file(item["dest"], int(item["size"]))]
        if not missing:
            return

        total_bytes = sum(int(item["size"]) for item in missing)
        done_bytes = 0
        cb = progress_callback
        callback_failed = False

        def report(label: str) -> None:
            nonlocal callback_failed
            if cb is None:
                return
            try:
                cb("download", int(done_bytes), int(max(total_bytes, 1)), label)
            except Exception as exc:  # noqa: BLE001 - progress is best-effort, the download is not
                # A broken progress sink must not abort a multi-GB transfer, but
                # it must not vanish either. Logged once per call: it fires per
                # chunk, and thousands of identical lines bury the log.
                if not callback_failed:
                    callback_failed = True
                    log.warning(
                        "FLUX Fill: progress callback failed during the weight download "
                        "(label=%r); further progress errors of this download are not logged",
                        label,
                        exc_info=exc,
                    )

        report("Подготовка загрузки модели…")
        for item in missing:
            name = os.path.basename(item["dest"])
            base = done_bytes

            def on_chunk(n: int) -> None:
                nonlocal done_bytes
                done_bytes = base + n
                report(f"Скачивание {name}")

            def on_retry(event: RetryEvent) -> None:
                report(retry_progress_label(name, event))

            _download_file_streaming(
                item["url"], item["dest"], int(item["size"]), on_chunk,
                expected_sha256=item["sha256"], on_retry=on_retry, should_cancel=should_cancel,
            )
            done_bytes = base + int(item["size"])
            report(f"Скачано {name}")

    # ---- pipeline ----
    def _ensure_pipeline_locked(self, normalized: dict[str, Any], model_key: str) -> Any:
        """Return the cached pipeline for `model_key`, building it if needed.

        Caller must hold `self._lock`. A pipeline built for another key is
        dropped and reported to the model manager first. Placement depends on
        `normalized["cpu_offload"]`: either accelerate model CPU offload (weights
        stay in RAM, one component on the GPU at a time) or a full move onto the
        selected discrete GPU. Both placements route through `rocm_mmap_transfer`
        and are unchanged off ROCm.

        # Raises
        `FileNotFoundError` if the selected GGUF quant or the diffusers
        components are missing from `side_models`.
        """
        if self._pipe is not None and self._active_key == model_key:
            return self._pipe

        prev = self._active_key
        self._pipe = None
        self._active_key = None
        self._publish_health_locked()
        _clear_torch_cache()
        if prev is not None:
            self._model_manager.mark_unloaded(prev)

        if normalized["miopen_fast"]:
            _apply_miopen_fast()

        import torch  # noqa: F401  (after MIOpen env is set)

        try:
            torch.backends.cudnn.benchmark = False
        except Exception as exc:  # noqa: BLE001 - an optional tuning knob, never a load failure
            log.warning("FLUX Fill: could not disable cudnn.benchmark: %s", exc)

        from diffusers import (
            FluxFillPipeline,
            FluxTransformer2DModel,
            GGUFQuantizationConfig,
        )

        quant = normalized["quant"]
        gguf = gguf_path(quant)
        if not _is_nonempty_file(gguf):
            raise FileNotFoundError(f"GGUF не найден: {gguf}")
        if not _components_present():
            raise FileNotFoundError(
                f"Компоненты Flux Fill не загружены: {_components_dir()}"
            )

        dtype = torch.bfloat16 if normalized["dtype"] == "bf16" else torch.float16
        device = _select_discrete_device()

        transformer = FluxTransformer2DModel.from_single_file(
            gguf,
            quantization_config=GGUFQuantizationConfig(compute_dtype=dtype),
            torch_dtype=dtype,
        )
        pipe = FluxFillPipeline.from_pretrained(
            _components_dir(),
            transformer=transformer,
            torch_dtype=dtype,
        )
        pipe.set_progress_bar_config(disable=True)
        if normalized["vae_tiling"]:
            try:
                pipe.vae.enable_tiling()
                pipe.vae.enable_slicing()
            except Exception as exc:  # noqa: BLE001 - optional memory saver, never a load failure
                log.warning("FLUX Fill: could not enable VAE tiling/slicing: %s", exc)
        if normalized["cpu_offload"] and getattr(device, "type", "cpu") != "cpu":
            _materialize_components_for_offload(pipe, device)
            pipe.enable_model_cpu_offload(device=str(device))
        else:
            # ROCm mmap->GPU staging (see the file header). The patch wraps the
            # stock `DiffusionPipeline.to` rather than replacing it, because that
            # method first does its own sequential/model-offload, bnb,
            # group-offload, `hf_device_map` and HPU bookkeeping and only then
            # calls `module.to(device, dtype)` per component. The patch reaches
            # that inner call because `ModelMixin.to` and `PreTrainedModel.to`
            # both end in `super().to(...)`, and `super()` resolves the patched
            # `nn.Module.to` class attribute at call time. Staging is per tensor
            # with immediate release, so peak host RSS grows by one tensor.
            with patched_module_to():
                pipe.to(device)

        self._pipe = pipe
        self._device = device
        self._active_key = model_key
        self._publish_health_locked()
        return pipe

    def _unload_key(self, model_key: str) -> bool:
        with self._lock:
            if self._pipe is None or self._active_key != model_key:
                return False
            return self.unload()

    # ---- generation ----
    def _generate_locked(
        self,
        pipe: Any,
        image_rgb: np.ndarray,
        mask_u8: np.ndarray,
        normalized: dict[str, Any],
        progress_callback: ProgressCb | None,
    ) -> np.ndarray:
        import numpy as np
        import torch
        from PIL import Image

        H0, W0 = image_rgb.shape[:2]
        mask = _dilate_mask(mask_u8, normalized["dilate"])

        work_rgb, work_mask, scaled = image_rgb, mask, False
        max_side = normalized["max_side"]
        if max_side and max(H0, W0) > max_side:
            scale = max_side / max(H0, W0)
            nw, nh = max(1, int(W0 * scale)), max(1, int(H0 * scale))
            work_rgb = np.asarray(Image.fromarray(image_rgb, "RGB").resize((nw, nh), Image.LANCZOS))
            work_mask = np.asarray(Image.fromarray(mask, "L").resize((nw, nh), Image.NEAREST))
            scaled = True

        rgb_pad, _ = _pad_to_multiple(work_rgb, 16)
        mask_pad, _ = _pad_to_multiple(work_mask, 16)
        Hh, Ww = rgb_pad.shape[:2]

        seed = int(normalized["seed"])
        generator = torch.Generator("cpu")
        if seed >= 0:
            generator = generator.manual_seed(seed)
        else:
            generator = generator.manual_seed(int.from_bytes(os.urandom(4), "little"))

        steps = int(normalized["steps"])
        cb = progress_callback
        callback_failed = False

        def _on_step(_pipe: Any, step: int, _t: Any, kwargs: dict[str, Any]):
            nonlocal callback_failed
            if cb is not None:
                try:
                    cb("generate", int(step) + 1, steps, "Генерация")
                except Exception as exc:  # noqa: BLE001 - progress is best-effort, generation is not
                    # Logged once: it fires per diffusion step.
                    if not callback_failed:
                        callback_failed = True
                        log.warning(
                            "FLUX Fill: progress callback failed at generation step %d; "
                            "further progress errors of this run are not logged",
                            int(step) + 1,
                            exc_info=exc,
                        )
            return kwargs

        if cb is not None:
            cb("generate", 0, steps, "Генерация")

        result = pipe(
            prompt=normalized["prompt"] or "",
            image=Image.fromarray(rgb_pad, "RGB"),
            mask_image=Image.fromarray(mask_pad, "L"),
            height=Hh,
            width=Ww,
            num_inference_steps=steps,
            guidance_scale=float(normalized["guidance"]),
            max_sequence_length=int(normalized["max_seq"]),
            generator=generator,
            callback_on_step_end=_on_step,
            callback_on_step_end_tensor_inputs=["latents"],
        ).images[0]

        out = np.asarray(result.convert("RGB"))[: work_rgb.shape[0], : work_rgb.shape[1]]
        if scaled:
            out = np.asarray(Image.fromarray(out, "RGB").resize((W0, H0), Image.LANCZOS))

        if normalized["seamless"] and mask.any():
            try:
                return _seamless_composite(image_rgb, out, mask)
            except Exception as exc:  # noqa: BLE001 - falls back to the plain feathered composite
                log.warning(
                    "FLUX Fill: seamless (Poisson) composite failed, using the feathered composite: %s",
                    exc,
                    exc_info=exc,
                )

        m = mask.astype(np.float32) / 255.0
        if normalized["feather"] > 0:
            from PIL import ImageFilter

            m = np.asarray(
                Image.fromarray((m * 255).astype(np.uint8), "L").filter(
                    ImageFilter.GaussianBlur(normalized["feather"])
                )
            ).astype(np.float32) / 255.0
        m = m[..., None]
        composed = image_rgb.astype(np.float32) * (1 - m) + out.astype(np.float32) * m
        return np.clip(composed, 0, 255).astype(np.uint8)


# =====================================================================
#  Download plumbing
# =====================================================================
def _build_download_plan(quant: str) -> list[dict[str, Any]]:
    """Returns [{url, dest, size, sha256}] for the GGUF quant + all component files.

    `sha256` is the listing's LFS digest (`None` for a non-LFS file) and is
    checked on the complete file before it is published. `size` is the length
    the hub listing announced, in bytes; it is the download's size gate (`0` only when the listing named the file without
    a size, which is logged). The listing is mandatory: without it there is no
    size to check a ~12 GB file against.

    # Errors
    `RuntimeError` when a repository listing cannot be fetched or the selected
    GGUF quant is not in its repository.
    """
    from huggingface_hub import HfApi, hf_hub_url

    token = os.environ.get("HF_TOKEN") or os.environ.get("HUGGING_FACE_HUB_TOKEN")
    api = HfApi(token=token)
    plan: list[dict[str, Any]] = []

    # GGUF transformer (single file).
    gname = gguf_filename(quant)
    try:
        info = api.model_info(GGUF_REPO, files_metadata=True)
    except Exception as exc:  # noqa: BLE001 - every hub failure is the same refusal here
        log.error("FLUX Fill: listing of %s failed: %s", GGUF_REPO, exc)
        raise RuntimeError(f"Не удалось получить список файлов {GGUF_REPO}: {exc}") from exc
    gsib = next((sib for sib in info.siblings or [] if sib.rfilename == gname), None)
    if gsib is None:
        raise RuntimeError(f"Файл {gname} не найден в репозитории {GGUF_REPO}.")
    plan.append({
        "url": hf_hub_url(GGUF_REPO, gname),
        "dest": gguf_path(quant),
        "size": _listed_size(GGUF_REPO, gsib),
        "sha256": hub_lfs_sha256(gsib),
    })

    # diffusers components from the open Flex.1-alpha repo.
    try:
        comp_info = api.model_info(COMPONENTS_REPO, files_metadata=True)
        siblings = comp_info.siblings or []
    except Exception as exc:
        raise RuntimeError(f"Не удалось получить список файлов {COMPONENTS_REPO}: {exc}") from exc
    for sib in siblings:
        path = sib.rfilename
        if not any(path == p or path.startswith(p) for p in COMPONENT_PREFIXES):
            continue
        plan.append({
            "url": hf_hub_url(COMPONENTS_REPO, path),
            "dest": os.path.join(_components_dir(), *path.split("/")),
            "size": _listed_size(COMPONENTS_REPO, sib),
            "sha256": hub_lfs_sha256(sib),
        })
    return plan


#: Format version of the cached plan file; another version is ignored and refetched.
_PLAN_CACHE_VERSION = 1


def _plan_cache_path(quant: str) -> str:
    """`<flux dir>/flux1-fill-dev-<quant>.listing.json`: the last resolved plan of `quant`."""
    return os.path.join(_flux_dir(), f"flux1-fill-dev-{quant}.listing.json")


def _save_cached_plan(quant: str, plan: list[dict[str, Any]]) -> None:
    """Persist `plan` (as `_build_download_plan` returned it) for offline runs; atomic.

    Destinations are stored relative to `_flux_dir()`, so a moved installation
    still finds its files. A write failure only costs the offline shortcut, so
    it is logged and not raised.
    """
    root = _flux_dir()
    payload = {
        "version": _PLAN_CACHE_VERSION,
        "quant": quant,
        "files": [
            {
                "url": item["url"],
                "rel": os.path.relpath(item["dest"], root),
                "size": int(item["size"]),
                "sha256": item["sha256"],
            }
            for item in plan
        ],
    }
    path = _plan_cache_path(quant)
    tmp = f"{path}.{os.getpid()}.tmp"
    try:
        with open(tmp, "w", encoding="utf-8") as handle:
            json.dump(payload, handle)
        os.replace(tmp, path)
    except OSError as exc:
        log.warning("FLUX Fill: could not cache the download plan at %s: %s", path, exc)
        try:
            os.unlink(tmp)
        except FileNotFoundError:
            pass
        except OSError as cleanup_exc:
            log.warning("FLUX Fill: could not remove %s: %s", tmp, cleanup_exc)


def _load_cached_plan(quant: str) -> list[dict[str, Any]] | None:
    """The plan `_save_cached_plan` stored for `quant`, or `None` (absent, unreadable, foreign).

    An unreadable or malformed cache is logged and ignored: the caller then
    fetches the listing as if there were none. A destination that would leave
    `_flux_dir()` (absolute, or with `..`) invalidates the whole cache.
    """
    path = _plan_cache_path(quant)
    try:
        with open(path, encoding="utf-8") as handle:
            payload = json.load(handle)
    except FileNotFoundError:
        return None
    except (OSError, ValueError) as exc:
        log.warning("FLUX Fill: ignoring the unreadable cached plan %s: %s", path, exc)
        return None
    root = _flux_dir()
    try:
        if payload["version"] != _PLAN_CACHE_VERSION or payload["quant"] != quant:
            log.info("FLUX Fill: ignoring the cached plan %s of another version or quant", path)
            return None
        plan = []
        for entry in payload["files"]:
            rel = str(entry["rel"])
            parts = rel.replace("\\", "/").split("/")
            if os.path.isabs(rel) or ".." in parts:
                raise ValueError(f"destination {rel!r} leaves the model directory")
            sha256 = entry["sha256"]
            plan.append({
                "url": str(entry["url"]),
                "dest": os.path.join(root, *parts),
                "size": int(entry["size"]),
                "sha256": None if sha256 is None else str(sha256),
            })
        if not plan:
            raise ValueError("the plan lists no file")
    except (KeyError, TypeError, ValueError) as exc:
        log.warning("FLUX Fill: ignoring the malformed cached plan %s: %s", path, exc)
        return None
    return plan


def _listed_size(repo: str, sib: Any) -> int:
    """The announced size of listing entry `sib`, `0` (logged) when the hub gave none."""
    size = int(getattr(sib, "size", 0) or 0)
    if size <= 0:
        log.warning(
            "FLUX Fill: %s announced no size for %s; only a body shorter than its own "
            "Content-Length can be detected for this file",
            repo,
            getattr(sib, "rfilename", "?"),
        )
    return size


def _download_file_streaming(
    url: str,
    dest: str,
    expected_size: int,
    on_chunk: Callable[[int], None],
    *,
    expected_sha256: str | None = None,
    on_retry: Callable[[RetryEvent], None] | None = None,
    should_cancel: Callable[[], bool] | None = None,
) -> bool:
    """Download `url` to `dest`, reporting cumulative bytes. Returns whether it ran.

    The bearer header, the serialization, the process-private `.part` staging
    file, the retry/resume loop, the size gate and the atomic publish all belong
    to `engines.model_download.download_bearer_to_path`: IPC dispatches onto a
    thread pool, so two first uses of the same quant can otherwise write into one
    staging file at the same time — a ~22 GB corruption window here. A file
    another thread already finished is skipped, not refetched; a failed transfer
    keeps its bytes so the next attempt resumes them.

    `expected_size` is the listing's size (`0` = unknown) and `expected_sha256`
    the listing's LFS digest (`None` for a non-LFS file): together they are the
    gate a staged file passes before it is published. The token stays THIS
    module's business: FLUX.1-Fill reads it from the process environment
    (`HF_TOKEN` / `HUGGING_FACE_HUB_TOKEN`), while the FLUX.2 klein downloader
    receives it as a request field, so the shared helper takes it as an argument
    and never looks the environment up itself.

    # Errors
    Propagates the helper's errors (`DownloadCancelled`, `DownloadError`
    subclasses, `requests.HTTPError`, `RuntimeError` for disk failures).
    """
    token = os.environ.get("HF_TOKEN") or os.environ.get("HUGGING_FACE_HUB_TOKEN")
    return download_bearer_to_path(
        url,
        dest,
        token=token,
        expected_size=expected_size,
        expected_sha256=expected_sha256,
        should_cancel=should_cancel,
        on_progress=lambda done, _expected: on_chunk(done),
        on_retry=on_retry,
    )


def _components_present() -> bool:
    """Heuristic: model_index.json + the four big weight files exist non-empty."""
    cd = _components_dir()
    required = (
        os.path.join(cd, "model_index.json"),
        os.path.join(cd, "vae", "diffusion_pytorch_model.safetensors"),
        os.path.join(cd, "text_encoder", "model.safetensors"),
        os.path.join(cd, "text_encoder_2", "model-00001-of-00002.safetensors"),
        os.path.join(cd, "tokenizer_2", "spiece.model"),
    )
    return all(_is_nonempty_file(p) for p in required)


def _is_nonempty_file(path: str) -> bool:
    try:
        return os.path.isfile(path) and os.path.getsize(path) > 0
    except OSError:
        return False


def _is_complete_file(path: str, expected_size: int) -> bool:
    """Whether `path` is on disk at `expected_size` bytes (non-empty when the size is unknown)."""
    if expected_size <= 0:
        return _is_nonempty_file(path)
    try:
        return os.path.isfile(path) and os.path.getsize(path) == expected_size
    except OSError:
        return False


# =====================================================================
#  Device / MIOpen (discrete GPU only — iGPU excluded)
# =====================================================================
def _select_discrete_device() -> Any:
    """The discrete GPU with the most memory as a `torch.device` (made current), else CPU.

    This service ignores `General.ai_device` by design (`honour_configured=False`);
    the iGPU rule and the choice belong to
    `runtime.torch_device.resolve_discrete_backend_device`.
    """
    import torch

    device = torch.device(resolve_discrete_backend_device("cuda", honour_configured=False))
    if device.type == "cuda":
        torch.cuda.set_device(device)
    return device


# Pipeline components loaded from safetensors, i.e. the ones diffusers hands out
# backed by a writable private file mapping. `transformer` is deliberately
# absent: it comes from GGUF, whose reader copies every tensor through
# `torch.from_numpy(tensor.data.copy())` into anonymous memory, so it never hits
# the amdkfd stall and must not be round-tripped through the GPU for nothing.
_MMAP_BACKED_COMPONENTS = ("vae", "text_encoder", "text_encoder_2")


def _largest_cpu_tensor(module: Any) -> Any:
    """Largest CPU-resident parameter or buffer of `module`, or `None`.

    Used as the probe for `_component_is_file_backed`. Only CPU tensors qualify:
    a component that already sits on the GPU has nothing left to re-home. Ties
    and empty modules are irrelevant to the caller, which only needs one
    representative tensor.
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
    trip in `_materialize_components_for_offload`. The check is not redundant
    with `mmap_staging_required()`: at `dtype: "fp16"` the loaders cast the BF16
    checkpoint on the host, which already materializes every tensor in anonymous
    memory, and round-tripping ~9.5 GiB of it through the GPU would cost seconds
    and a ~9.2 GiB VRAM peak for nothing.

    The largest CPU tensor is representative: a component is loaded in one pass
    from its own file, so its weights are either all mapped or all copied.
    Returns `False` when the module has no CPU tensors left (nothing to re-home)
    or when the file-backing detection is unavailable.
    """
    probe = _largest_cpu_tensor(module)
    if probe is None:
        return False
    return tensor_needs_staging(probe)


def _materialize_components_for_offload(pipe: Any, device: Any) -> None:
    """Re-home the safetensors components in anonymous host memory before offload.

    `enable_model_cpu_offload` never moves a component to the GPU itself: it
    leaves every module on the CPU and installs an accelerate `CpuOffload` hook
    that does `module.to(<gpu>)` lazily from inside the forward pass. On ROCm
    that first lazy move copies straight out of the safetensors mapping and hits
    the amdkfd copy-on-write stall (~1-2 s per tensor of >=1 MiB; at the default
    BF16 dtype these three components carry 284 such tensors, ~9.5 GiB), only it
    happens during generation instead of during load. `patched_module_to` cannot
    be held there: it is process-global and its contract forbids wrapping
    inference.

    So the weights are re-homed up front: one staged CPU->GPU copy per component
    immediately followed by a move back, which leaves the resident CPU copy in
    freshly allocated anonymous memory. Every later lazy move is then a plain
    fast transfer. Peak VRAM is one component, exactly what model CPU offload
    peaks at during generation anyway, and the anonymous host copy is what
    accelerate would end up with after the first offload cycle regardless.

    Skipped entirely unless `mmap_staging_required()` holds (ROCm/HIP build on
    posix), and per component unless its weights are actually file-backed (see
    `_component_is_file_backed`).

    A failed re-homing is a lost optimization, not a lost model: it is logged
    with context and the load continues, so the first generation merely pays the
    stall. Components left on the GPU by a partial failure are pulled back by
    `enable_model_cpu_offload`, which starts with its own `to("cpu")`.
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
                "Flux Fill: could not re-home component %r in anonymous host memory "
                "before CPU offload (%s). The model still works, but on ROCm the "
                "first generation may stall in the mmap->GPU weight copy.",
                name,
                exc,
            )
            break
        # Release the component's device blocks before the next, larger one is
        # staged: the allocator does not serve a ~9 GiB T5 request out of the
        # small blocks left behind by the VAE and would reserve on top of them,
        # which turns a fitting model into an OOM on a 12 GiB card.
        _clear_torch_cache()
        rehomed.append(name)

    _clear_torch_cache()
    if rehomed:
        log.info(
            "Flux Fill: re-homed %s in anonymous host memory in %.2f s before enabling "
            "model CPU offload (ROCm mmap->GPU stall workaround).",
            ", ".join(rehomed),
            time.perf_counter() - started_at,
        )
    if skipped:
        log.debug(
            "Flux Fill: %s already live in anonymous host memory (no file mapping); "
            "skipped the pre-offload round trip.",
            ", ".join(skipped),
        )


def _apply_miopen_fast() -> None:
    """MIOpen immediate mode — set before torch import (mirrors rocm_runtime.py)."""
    os.environ.setdefault("MIOPEN_FIND_MODE", "2")
    cache_root = os.path.join(os.path.dirname(os.path.dirname(_flux_dir())), ".cache", "miopen")
    try:
        user_db = os.path.join(cache_root, "user_db")
        kernels = os.path.join(cache_root, "kernels")
        os.makedirs(user_db, exist_ok=True)
        os.makedirs(kernels, exist_ok=True)
        os.environ.setdefault("MIOPEN_USER_DB_PATH", user_db)
        os.environ.setdefault("MIOPEN_CUSTOM_CACHE_DIR", kernels)
    except OSError:
        pass


# =====================================================================
#  Image / mask helpers
# =====================================================================
def _decode_image_rgb(image_bytes: bytes) -> np.ndarray:
    import numpy as np
    from PIL import Image

    with Image.open(io.BytesIO(image_bytes)) as img:
        return np.ascontiguousarray(np.array(img.convert("RGB"), dtype=np.uint8))


def _decode_mask(mask_bytes: bytes, *, expected_hw: tuple[int, int]) -> np.ndarray:
    import numpy as np
    from PIL import Image

    with Image.open(io.BytesIO(mask_bytes)) as img:
        arr = np.array(img)
    if arr.ndim == 3:
        mask = arr[..., 3] if arr.shape[2] >= 4 else np.max(arr[..., :3], axis=2)
    elif arr.ndim == 2:
        mask = arr
    else:
        raise ValueError("Некорректная маска: ожидается 2D/3D массив")
    mask = np.ascontiguousarray(mask.astype(np.uint8))
    if tuple(mask.shape[:2]) != tuple(expected_hw):
        raise ValueError(
            f"Размер маски {mask.shape[1]}x{mask.shape[0]} не совпадает с изображением "
            f"{expected_hw[1]}x{expected_hw[0]}"
        )
    return np.where(mask > 0, 255, 0).astype(np.uint8)


def _dilate_mask(mask: np.ndarray, dilate: int) -> np.ndarray:
    if dilate <= 0:
        return mask
    try:
        import cv2
        import numpy as np

        k = 2 * int(dilate) + 1
        kernel = cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (k, k))
        return cv2.dilate(mask, kernel, iterations=1)
    except Exception:
        from PIL import Image, ImageFilter
        import numpy as np

        m = Image.fromarray(mask, "L")
        extra = dilate
        while extra > 0:
            m = m.filter(ImageFilter.MaxFilter(min(2 * extra + 1, 31)))
            extra -= 15
        return np.asarray(m)


def _pad_to_multiple(arr: np.ndarray, mult: int = 16):
    import numpy as np

    h, w = arr.shape[:2]
    ph = (mult - h % mult) % mult
    pw = (mult - w % mult) % mult
    if ph == 0 and pw == 0:
        return arr, (h, w)
    if arr.ndim == 3:
        return np.pad(arr, ((0, ph), (0, pw), (0, 0)), mode="edge"), (h, w)
    return np.pad(arr, ((0, ph), (0, pw)), mode="constant"), (h, w)


def _seamless_composite(orig: np.ndarray, gen: np.ndarray, mask: np.ndarray) -> np.ndarray:
    """Poisson (cv2.seamlessClone) per mask component — matches the patch tone to
    the surroundings, removing the darker/lighter seam left by Flux Fill."""
    import cv2
    import numpy as np

    pad = 12
    o = cv2.copyMakeBorder(orig, pad, pad, pad, pad, cv2.BORDER_REPLICATE)
    g = cv2.copyMakeBorder(gen, pad, pad, pad, pad, cv2.BORDER_REPLICATE)
    m = cv2.copyMakeBorder(mask, pad, pad, pad, pad, cv2.BORDER_CONSTANT, value=0)
    n, labels = cv2.connectedComponents((m > 127).astype(np.uint8))
    result = o.copy()
    for i in range(1, n):
        comp = (labels == i).astype(np.uint8) * 255
        # OpenCV places the patch top-left at (center - boundingRect.size//2). To
        # land it exactly in place (no 1px shift on even-sized bboxes) the center
        # must be (x + w//2, y + h//2) — NOT the bbox midpoint.
        x, y, w, h = cv2.boundingRect(comp)
        if w < 2 or h < 2:
            continue
        center = (int(x + w // 2), int(y + h // 2))
        try:
            result = cv2.seamlessClone(g, result, comp, center, cv2.NORMAL_CLONE)
        except cv2.error:
            a = (comp.astype(np.float32) / 255.0)[..., None]
            result = (result.astype(np.float32) * (1 - a) + g.astype(np.float32) * a).astype(np.uint8)
    return np.ascontiguousarray(result[pad:-pad, pad:-pad])


def _encode_png_bytes_rgb(image_rgb: np.ndarray) -> bytes:
    import numpy as np
    from PIL import Image

    arr = np.ascontiguousarray(image_rgb.astype(np.uint8))
    with io.BytesIO() as buf:
        Image.fromarray(arr, "RGB").save(buf, format="PNG")
        return buf.getvalue()


#: Allocator-cache release with this service's policy (owner: `runtime.torch_device`);
#: patched by name in the test suite.
_clear_torch_cache = functools.partial(clear_torch_cache, owner="FLUX Fill", include_mps=False)


# =====================================================================
#  Coercion helpers
# =====================================================================
def _to_int(value: Any, default: int) -> int:
    try:
        if isinstance(value, bool):
            return default
        return int(value)
    except Exception:
        return default


def _to_bool(value: Any, default: bool) -> bool:
    if isinstance(value, bool):
        return value
    if value is None:
        return default
    if isinstance(value, (int, float)):
        return bool(value)
    s = str(value).strip().lower()
    if s in {"1", "true", "yes", "on"}:
        return True
    if s in {"0", "false", "no", "off"}:
        return False
    return default


def _clamp_int(value: Any, *, default: int, low: int, high: int) -> int:
    return max(low, min(high, _to_int(value, default)))


def _clamp_float(value: Any, *, default: float, low: float, high: float) -> float:
    try:
        out = default if isinstance(value, bool) else float(value)
    except Exception:
        out = default
    return max(low, min(high, out))
