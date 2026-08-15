"""
File: modules/ai_backend/watermark/service.py

Purpose:
`WatermarkRemovalService` — visible-watermark detection and removal for the
Python AI backend (`watermark.detect` / `.remove` / `.status` / `.unload`).
Three upstream networks are supported: SLBR (default), WDNet and SplitNet.

Main responsibilities:
- own the Google-Drive weight download into
  `ManhwaStudio_AI_Models/side_models/WatermarkRemoval/<model>/`, with byte-level
  progress and an integrity gate; the serialization, the process-private `.part`
  staging file and the atomic publish come from `engines/model_download.py`,
  which `inpaint/flux_fill.py` shares;
- lazily build one resident network at a time under an `RLock`, leased from the
  shared `LoadedModelManager` with key `watermark:<model>:<device>`;
- `detect_mask_bytes`: a single whole-image pass on a downscaled square copy,
  producing an L8 mask at the SOURCE resolution — the primary product, meant to
  be fed to one of the project's existing inpainters;
- `remove_watermark_bytes`: the experimental direct pass, tiled with a
  cosine-feathered blend and the upstream per-tile composition.

Key structures:
- `WatermarkRemovalService`
- `WEIGHTS` — per-model checkpoint spec (Drive id, file name, expected size).

Key functions:
- `normalize_detect_params()`, `normalize_remove_params()`

Notes:
- The network code itself is NOT vendored (no upstream repository has a
  LICENSE); it is downloaded and imported by `code_fetch.py`.
- Torch is imported lazily, inside the methods that need it.
- The SHA-256 of the three checkpoints is not known yet (Google Drive was
  unreachable when the manifest was captured), so `_WEIGHT_SHA256` holds `None`
  for all three, meaning "unverified". The magic-byte gate always runs and is
  what rejects an HTML interstitial silently saved as a `.pth.tar`.
- Weight moves go through `runtime.rocm_mmap_transfer.move_module_to` (Form 1);
  the download always completes before that call.
"""

from __future__ import annotations

import io
import logging
import re
import threading
from pathlib import Path
from typing import TYPE_CHECKING, Any, Callable

if TYPE_CHECKING:
    import numpy as np

try:
    from ai_device import AIDevice
except Exception:  # pragma: no cover - resolved differently under the two roots
    from modules.ai_device import AIDevice

try:
    from config import UserConfig
except Exception:  # pragma: no cover - config is always importable in-app
    UserConfig = None

from ..engines.model_download import download_to_path, stream_response_to_file
from ..runtime.model_manager import LoadedModelManager
from ..runtime.rocm_mmap_transfer import move_module_to
from . import code_fetch

log = logging.getLogger(__name__)

#: Progress callback: `(phase, step, total, label)`, phase in {"download", "generate"}.
ProgressCb = Callable[[str, int, int, str], None]

#: Catalog order; `MODEL_IDS[0]` is the default.
MODEL_IDS: tuple[str, ...] = code_fetch.MODEL_IDS
DEFAULT_MODEL = "slbr"

#: Allowed values of the `downscale_to` detect parameter.
DOWNSCALE_OPTIONS: tuple[int, ...] = (256, 512, 768)
DEFAULT_DOWNSCALE = 512

# Tiles must be square and divisible by 16 (SLBR/SplitNet), so `tile` is snapped
# onto that grid; `overlap` can never eat a whole tile.
MIN_TILE = 128
MAX_TILE = 2048
DEFAULT_TILE = 512
DEFAULT_OVERLAP = 64

_DRIVE_URL = "https://drive.usercontent.google.com/download?id={file_id}&export=download"
_DOWNLOAD_TIMEOUT_S = 120


class _WeightSpec:
    """Checkpoint of one model: where it comes from and what it is called on disk."""

    __slots__ = ("file_name", "drive_id", "size")

    def __init__(self, file_name: str, drive_id: str, size: int) -> None:
        self.file_name = file_name
        self.drive_id = drive_id
        #: Expected size in bytes, used only for progress accounting - Google
        #: Drive does not always send a Content-Length.
        self.size = size


#: Per-model checkpoint sources (dev-docs/watermark_removal_plan.md §3.4).
WEIGHTS: dict[str, _WeightSpec] = {
    "slbr": _WeightSpec("model_best.pth.tar", "1uTCzubnWZtu3HIXaK8xsXX-7x302ss13", 85_674_000),
    "wdnet": _WeightSpec("WDNet_G.pkl", "1Tv0iM3ZbM0j3akP9uI1myC18IUnyDNjL", 80_400_000),
    "splitnet": _WeightSpec(
        "27kpng_model_best.pth.tar", "1KpSJ6385CHN6WlAINqB3CYrJdleQTJBc", 130_600_000
    ),
}

#: Expected SHA-256 per model, or `None` for "not captured yet, unverified".
#: These are third-party Google Drive files whose digests could not be recorded
#: when the manifest was written. A `None` entry disables the digest comparison
#: ONLY - `_validate_checkpoint_magic` still runs on every download and on every
#: load. Fill an entry in as soon as a real download has been hashed; never
#: invent a value.
_WEIGHT_SHA256: dict[str, str | None] = {"slbr": None, "wdnet": None, "splitnet": None}

# Accepted first bytes of a Torch checkpoint: the modern zip container, or a
# protocol>=2 pickle stream (all three of these files are legacy non-zip saves).
_ZIP_MAGIC = b"PK\x03\x04"
_PICKLE_PROTO_OPCODE = b"\x80"


# =====================================================================
#  Parameter normalization
# =====================================================================
def normalize_detect_params(params: dict[str, Any] | None) -> dict[str, Any]:
    """Validate and clamp the `watermark.detect` parameters.

    Returns `{model, downscale_to, threshold, dilate_px}`. `downscale_to` is
    snapped to one of `DOWNSCALE_OPTIONS`, `threshold` to `[0, 1]` and
    `dilate_px` to `[0, 64]`.

    # Errors
    Raises `ValueError` when `model` is present but is not a known model id. An
    absent or empty `model` means `DEFAULT_MODEL`; an unknown one is never
    silently replaced.
    """
    merged: dict[str, Any] = dict(params) if isinstance(params, dict) else {}
    downscale = _to_int(merged.get("downscale_to"), DEFAULT_DOWNSCALE)
    return {
        "model": _normalize_model(merged.get("model")),
        "downscale_to": downscale if downscale in DOWNSCALE_OPTIONS else DEFAULT_DOWNSCALE,
        "threshold": _clamp_float(merged.get("threshold"), default=0.5, low=0.0, high=1.0),
        "dilate_px": _clamp_int(merged.get("dilate_px"), default=4, low=0, high=64),
    }


def normalize_remove_params(params: dict[str, Any] | None) -> dict[str, Any]:
    """Validate and clamp the `watermark.remove` parameters.

    Returns `{model, tile, overlap, threshold, dilate_px}`. `tile` is clamped to
    `[MIN_TILE, MAX_TILE]` and snapped DOWN to a multiple of 16 (SLBR and
    SplitNet reject anything else); `overlap` is clamped to `[0, tile // 2]` so
    a tile can never be fully consumed by its own feather.

    `threshold` and `dilate_px` shape only the mask returned alongside the
    cleaned image — the per-tile composition uses the raw soft mask, exactly as
    upstream does.

    # Errors
    Raises `ValueError` for an unknown `model`.
    """
    merged: dict[str, Any] = dict(params) if isinstance(params, dict) else {}
    tile = _clamp_int(merged.get("tile"), default=DEFAULT_TILE, low=MIN_TILE, high=MAX_TILE)
    tile = max(MIN_TILE, (tile // 16) * 16)
    overlap = _clamp_int(merged.get("overlap"), default=DEFAULT_OVERLAP, low=0, high=tile // 2)
    return {
        "model": _normalize_model(merged.get("model")),
        "tile": tile,
        "overlap": overlap,
        "threshold": _clamp_float(merged.get("threshold"), default=0.5, low=0.0, high=1.0),
        "dilate_px": _clamp_int(merged.get("dilate_px"), default=4, low=0, high=64),
    }


def _normalize_model(value: Any) -> str:
    """`DEFAULT_MODEL` for an absent/empty value; raise for an unknown id."""
    name = str(value or "").strip().lower()
    if not name:
        return DEFAULT_MODEL
    if name not in MODEL_IDS:
        raise ValueError(
            f"Неизвестная модель удаления водяных знаков: {name!r}. "
            f"Доступны: {', '.join(MODEL_IDS)}"
        )
    return name


# =====================================================================
#  Paths
# =====================================================================
def weights_path(model_id: str) -> Path:
    """On-disk location of `model_id`'s checkpoint."""
    if model_id not in WEIGHTS:
        raise ValueError(f"Неизвестная модель удаления водяных знаков: {model_id!r}")
    return code_fetch.model_dir(model_id) / WEIGHTS[model_id].file_name


def are_weights_ready(model_id: str) -> bool:
    """Whether `model_id`'s checkpoint exists and is non-empty."""
    try:
        path = weights_path(model_id)
        return path.is_file() and path.stat().st_size > 0
    except (OSError, ValueError):
        return False


# =====================================================================
#  Service
# =====================================================================
class WatermarkRemovalService:
    """Lazy-loading visible-watermark detector/remover for the `watermark.*` methods.

    One network is resident at a time, guarded by `self._lock` and leased from
    the shared `LoadedModelManager` under the key `watermark:<model>:<device>`.
    Construction is cheap and imports nothing heavy.
    """

    def __init__(self, model_manager: LoadedModelManager) -> None:
        self._lock = threading.RLock()
        self._model_manager = model_manager
        self._net: Any = None
        self._active_key: str | None = None
        self._active_model: str | None = None
        self._active_device: str = "cpu"
        self._last_error: str | None = None

    # ---- status / health ----
    def health(self) -> dict[str, Any]:
        """Current residency snapshot for the `health` IPC method."""
        with self._lock:
            return {
                "ready": self._net is not None,
                "model": self._active_model or DEFAULT_MODEL,
                "device": self._active_device,
                "active_key": self._active_key,
                "last_error": self._last_error,
            }

    def status(self) -> dict[str, Any]:
        """Model catalog plus what is already on disk (weights and network code)."""
        models = [
            {
                "id": model_id,
                "weights_ready": are_weights_ready(model_id),
                "code_ready": code_fetch.is_code_ready(model_id),
            }
            for model_id in MODEL_IDS
        ]
        return {
            "models": models,
            "default_model": DEFAULT_MODEL,
            "downloaded_models": [m["id"] for m in models if m["weights_ready"]],
            "code_ready_models": [m["id"] for m in models if m["code_ready"]],
        }

    def unload(self) -> bool:
        """Drop the resident network; `False` when nothing was loaded."""
        with self._lock:
            if self._net is None:
                return False
            key = self._active_key
            self._net = None
            self._active_key = None
            self._active_model = None
            _clear_torch_cache()
            if key is not None:
                self._model_manager.mark_unloaded(key)
            return True

    # ---- main entries ----
    def detect_mask_bytes(
        self,
        image_png: bytes,
        params: dict[str, Any] | None = None,
        progress_callback: ProgressCb | None = None,
    ) -> dict[str, Any]:
        """Predict the watermark mask of `image_png` at its SOURCE resolution.

        The image is downscaled so its long side is `downscale_to`, reflect-padded
        to a square multiple of 16 and run through the network ONCE — never
        tiled: the mask branch has to see the whole watermark, and a tile of a
        webtoon page usually cannot. The prediction is cropped, upscaled back
        bilinearly, thresholded and dilated.

        Returns `{mask_png (L8), model, device, source_size [w, h], mask_coverage}`
        where `mask_coverage` is the fraction of source pixels inside the mask.

        # Errors
        Raises `ValueError` for bad parameters or an undecodable image,
        `RuntimeError` for a failed download/load, `FileNotFoundError` when the
        checkpoint is missing after a download attempt.
        """
        import numpy as np

        normalized = normalize_detect_params(params)
        image_rgb = _decode_image_rgb(image_png)
        source_h, source_w = image_rgb.shape[:2]
        model_id = normalized["model"]

        self.ensure_model_assets(model_id, progress_callback)

        work_rgb = _downscale_long_side(image_rgb, normalized["downscale_to"])
        padded, (crop_h, crop_w) = _pad_square_multiple(
            work_rgb, code_fetch.input_size_multiple(model_id)
        )

        _emit(progress_callback, "generate", 0, 1, "Поиск водяного знака")
        _clean, soft_mask = self._lease_and_run(
            model_id, lambda net: _forward_numpy(net, model_id, padded)
        )
        _emit(progress_callback, "generate", 1, 1, "Поиск водяного знака")

        soft_mask = soft_mask[:crop_h, :crop_w]
        upscaled = _resize_mask_float(soft_mask, source_w, source_h)
        binary = _binarize_and_dilate(upscaled, normalized["threshold"], normalized["dilate_px"])
        coverage = float(np.count_nonzero(binary)) / float(max(source_h * source_w, 1))

        return {
            "mask_png": _encode_png_bytes_l8(binary),
            "model": model_id,
            "device": self._active_device,
            "source_size": [int(source_w), int(source_h)],
            "mask_coverage": coverage,
        }

    def remove_watermark_bytes(
        self,
        image_png: bytes,
        params: dict[str, Any] | None = None,
        progress_callback: ProgressCb | None = None,
    ) -> dict[str, Any]:
        """Run the direct (experimental) network cleaning pass over `image_png`.

        The image is covered by square tiles of `tile` px (a multiple of 16)
        overlapping by `overlap` px, blended with a cosine feather normalized by
        the accumulated weight, and each tile is composed as
        `out = pred * mask + input * (1 - mask)`. Skipping that composition is a
        known upstream defect: the loss never constrains the region outside the
        mask, so the raw prediction carries colored artifacts there.

        Emits one `generate` progress frame per tile.

        Returns `{image_png (RGB), mask_png (L8), model, device, source_size [w, h]}`.

        # Errors
        Same as `detect_mask_bytes`.
        """
        normalized = normalize_remove_params(params)
        image_rgb = _decode_image_rgb(image_png)
        source_h, source_w = image_rgb.shape[:2]
        model_id = normalized["model"]

        self.ensure_model_assets(model_id, progress_callback)

        clean_rgb, soft_mask = self._run_tiled(
            model_id,
            image_rgb,
            tile=normalized["tile"],
            overlap=normalized["overlap"],
            progress_callback=progress_callback,
        )

        binary = _binarize_and_dilate(soft_mask, normalized["threshold"], normalized["dilate_px"])
        return {
            "image_png": _encode_png_bytes_rgb(clean_rgb),
            "mask_png": _encode_png_bytes_l8(binary),
            "model": model_id,
            "device": self._active_device,
            "source_size": [int(source_w), int(source_h)],
        }

    # ---- assets ----
    def ensure_model_assets(
        self, model_id: str, progress_callback: ProgressCb | None = None
    ) -> None:
        """Make sure both the network code and the checkpoint of `model_id` are on disk.

        Always completes BEFORE any weight is moved to the GPU: the ROCm staging
        context must never wrap a download.

        # Errors
        Raises `ValueError` for an unknown model and `RuntimeError` when a
        download or an integrity check fails.
        """
        model_id = _normalize_model(model_id)
        code_fetch.ensure_model_code(model_id, progress_callback)
        self._ensure_weights(model_id, progress_callback)

    def _ensure_weights(self, model_id: str, progress_callback: ProgressCb | None) -> None:
        """Download `model_id`'s checkpoint from Google Drive if it is not present.

        Serialization per destination, the process-private `.part` staging file
        and the atomic publish belong to `engines.model_download`, so two
        concurrent first-use requests cannot interleave their bytes into one
        destination and the loser skips the 80-130 MiB refetch. Every completed
        download is gated by `_validate_checkpoint_magic` and, when a digest is
        known, by SHA-256 — before it becomes the checkpoint.

        # Errors
        Raises `RuntimeError` on a transport failure, on an HTML interstitial
        saved instead of a checkpoint, or on a digest mismatch.
        """
        spec = WEIGHTS[model_id]
        total = max(spec.size, 1)
        label = f"Скачивание {spec.file_name}"

        def on_chunk(done: int, expected: int) -> None:
            _emit(progress_callback, "download", done, max(expected, total), label)

        def fetch(staging: Path) -> None:
            log.info(
                "watermark: downloading checkpoint %r (%s) from Google Drive id %s",
                model_id,
                spec.file_name,
                spec.drive_id,
            )
            _emit(progress_callback, "download", 0, total, "Подготовка загрузки весов…")
            _download_from_google_drive(spec.drive_id, staging, on_chunk)

        def verify(staging: Path) -> None:
            _validate_checkpoint_magic(staging, model_id)
            _verify_optional_sha256(staging, model_id)

        if download_to_path(weights_path(model_id), fetch, verify=verify):
            _emit(progress_callback, "download", total, total, f"Скачано {spec.file_name}")

    # ---- model residency ----
    def _ensure_model_locked(self, model_id: str, device: str, model_key: str) -> Any:
        """Return the resident network for `model_key`, loading it if needed.

        Caller must hold `self._lock`. A network loaded under another key is
        dropped and reported to the model manager first. The weight move uses
        `move_module_to` (Form 1) — a strict no-op off ROCm.

        # Errors
        Raises `FileNotFoundError` when the checkpoint is missing and
        `RuntimeError` when the checkpoint cannot be read into the network.
        """
        if self._net is not None and self._active_key == model_key:
            return self._net

        previous = self._active_key
        self._net = None
        self._active_key = None
        self._active_model = None
        _clear_torch_cache()
        if previous is not None:
            self._model_manager.mark_unloaded(previous)

        path = weights_path(model_id)
        if not (path.is_file() and path.stat().st_size > 0):
            raise FileNotFoundError(
                f"Не найден checkpoint модели «{model_id}»: {path}. Скачайте веса модели."
            )
        _validate_checkpoint_magic(path, model_id)

        net = code_fetch.build_network(model_id)
        state_dict = _load_state_dict(path, model_id)
        _load_into_network(net, state_dict, model_id)
        net.eval()
        move_module_to(net, device)

        self._net = net
        self._active_key = model_key
        self._active_model = model_id
        self._active_device = device
        log.info("watermark: model %r ready on %s (key %s)", model_id, device, model_key)
        return net

    def _unload_key(self, model_key: str) -> bool:
        """Unload only if `model_key` is the currently resident key."""
        with self._lock:
            if self._net is None or self._active_key != model_key:
                return False
            return self.unload()

    def _lease_and_run(self, model_id: str, run: Callable[[Any], Any]) -> Any:
        """Lease `model_id` from the model manager, load it, and call `run(net)`.

        The caller must NOT hold `self._lock`: `begin_model_use` may block while
        another thread's eviction callback waits for this service's lock, so the
        lease is taken first and `self._lock` only afterwards — the same order
        every other inpaint service uses.

        Implements the full four-call protocol (`begin_model_use` ->
        `mark_loaded` / `mark_load_failed` -> `release`), so an idle model stays
        evictable by another service. `mark_load_failed` is reserved for a
        failure of the LOAD itself: once `_ensure_model_locked` has returned, the
        network is resident and is registered as such before `run` is called, so
        a failure inside `run` leaves it counted and evictable instead of
        occupying VRAM off the manager's books.
        """
        device = _resolve_selected_backend_device(self._active_device)
        model_key = f"watermark:{model_id}:{device}"
        lease = self._model_manager.begin_model_use(
            model_key, unload_callback=lambda: self._unload_key(model_key)
        )
        with self._lock:
            try:
                try:
                    net = self._ensure_model_locked(model_id, device, model_key)
                except Exception:
                    if lease.needs_load:
                        lease.mark_load_failed()
                    raise
                if lease.needs_load:
                    lease.mark_loaded(unload_callback=lambda: self._unload_key(model_key))
                result = run(net)
                self._last_error = None
                return result
            except Exception as exc:
                self._last_error = str(exc)
                log.exception("watermark: %r failed on %s", model_id, device)
                raise
            finally:
                lease.release()

    def _run_tiled(
        self,
        model_id: str,
        image_rgb: np.ndarray,
        *,
        tile: int,
        overlap: int,
        progress_callback: ProgressCb | None,
    ) -> tuple[np.ndarray, np.ndarray]:
        """Tiled cleaning pass. Returns `(clean RGB uint8, soft mask float32)`.

        The image is reflect-padded to at least one full tile, every tile is
        composed over its own input, and the results are accumulated with a
        cosine feather and divided by the accumulated weight so the blend is an
        exact partition of unity — including at the borders, where a tile has no
        neighbour.
        """
        import numpy as np

        source_h, source_w = image_rgb.shape[:2]
        padded, _ = _pad_reflect(image_rgb, max(source_h, tile), max(source_w, tile))
        pad_h, pad_w = padded.shape[:2]

        positions = _plan_tiles(pad_h, pad_w, tile, overlap)
        window = _feather_window(tile, overlap)
        weight_2d = np.outer(window, window).astype(np.float32)

        acc_rgb = np.zeros((pad_h, pad_w, 3), dtype=np.float32)
        acc_mask = np.zeros((pad_h, pad_w), dtype=np.float32)
        acc_weight = np.zeros((pad_h, pad_w), dtype=np.float32)
        total = len(positions)

        def run(net: Any) -> None:
            for index, (top, left) in enumerate(positions):
                patch = padded[top : top + tile, left : left + tile]
                clean, mask = _forward_numpy(net, model_id, patch)
                # Upstream post-step, applied per tile: the training loss never
                # constrains the area outside the mask, so the raw prediction is
                # only trustworthy inside it.
                soft = mask[..., None]
                composed = clean.astype(np.float32) * soft + patch.astype(np.float32) * (1.0 - soft)
                acc_rgb[top : top + tile, left : left + tile] += composed * weight_2d[..., None]
                acc_mask[top : top + tile, left : left + tile] += mask * weight_2d
                acc_weight[top : top + tile, left : left + tile] += weight_2d
                _emit(
                    progress_callback,
                    "generate",
                    index + 1,
                    total,
                    f"Плитка {index + 1}/{total}",
                )

        _emit(progress_callback, "generate", 0, total, f"Плитка 0/{total}")
        self._lease_and_run(model_id, run)

        safe_weight = np.maximum(acc_weight, 1e-6)
        blended = acc_rgb / safe_weight[..., None]
        blended_mask = acc_mask / safe_weight
        clean_rgb = np.clip(blended[:source_h, :source_w], 0.0, 255.0).astype(np.uint8)
        soft_mask = np.clip(blended_mask[:source_h, :source_w], 0.0, 1.0).astype(np.float32)
        return np.ascontiguousarray(clean_rgb), np.ascontiguousarray(soft_mask)


# =====================================================================
#  Inference helpers
# =====================================================================
def _forward_numpy(net: Any, model_id: str, rgb: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    """Run `net` on one RGB uint8 array. Returns `(clean uint8, soft mask float32)`.

    Inputs are fed as `[0, 1]` RGB with no mean/std normalization — that is what
    all three upstream inference paths do, and what their `clamp(0, 1)` and
    long-skip additions assume.
    """
    import numpy as np
    import torch

    device = next(net.parameters()).device
    tensor = torch.from_numpy(np.ascontiguousarray(rgb.astype(np.float32) / 255.0))
    tensor = tensor.permute(2, 0, 1).unsqueeze(0).to(device)

    with torch.no_grad():
        outputs = net(tensor)
    clean_t, mask_t = _select_outputs(model_id, outputs)

    clean = clean_t.detach().float().clamp(0.0, 1.0).cpu().squeeze(0).permute(1, 2, 0).numpy()
    mask = mask_t.detach().float().clamp(0.0, 1.0).cpu().squeeze(0).squeeze(0).numpy()
    clean_u8 = np.clip(np.round(clean * 255.0), 0, 255).astype(np.uint8)
    return np.ascontiguousarray(clean_u8), np.ascontiguousarray(mask.astype(np.float32))


def _select_outputs(model_id: str, outputs: Any) -> tuple[Any, Any]:
    """Pick `(clean image, mask)` out of a model's differently shaped output tuple.

    - SLBR returns `([refined_bg, coarse_bg], [mask, ...aux], [watermark])`;
    - SplitNet returns `([refined, coarse], mask, watermark)`;
    - WDNet returns `(image, mask, alpha, watermark, intermediate)`.

    # Errors
    Raises `RuntimeError` when the output does not have the expected shape,
    which is how a silently changed upstream file surfaces.
    """
    try:
        if model_id == "slbr":
            images, masks, _watermark = outputs
            return images[0], masks[0]
        if model_id == "splitnet":
            images, mask, _watermark = outputs
            return images[0], mask
        if model_id == "wdnet":
            image, mask = outputs[0], outputs[1]
            return image, mask
    except (TypeError, ValueError, IndexError) as exc:
        raise RuntimeError(
            f"Сеть «{model_id}» вернула неожиданный набор выходов. "
            "Возможно, изменился загруженный код модели."
        ) from exc
    raise ValueError(f"Неизвестная модель удаления водяных знаков: {model_id!r}")


def _load_state_dict(path: Path, model_id: str) -> dict[str, Any]:
    """Read `path` as a Torch checkpoint and return its parameter dictionary.

    Loads with `map_location="cpu"` (the tensors are tagged `cuda:0` upstream)
    and an explicit `weights_only=True`, unwraps a `state_dict` entry when
    present and strips a `module.` prefix left by `DataParallel`.

    # Errors
    Raises `RuntimeError` with an actionable message when torch's restricted
    unpickler (`weights_only=True`, the default since torch 2.6) refuses the
    file, and when the payload is not a mapping. The restriction is never
    bypassed with a blanket `weights_only=False`.
    """
    import torch

    try:
        # `weights_only=True` is passed explicitly, not inherited from the
        # installed torch's default: it only became the default in torch 2.6,
        # and these are third-party files whose pickle stream must never be
        # executed just because an older runtime is in use.
        checkpoint = torch.load(path, map_location="cpu", weights_only=True)
    except Exception as exc:
        if _is_restricted_unpickler_error(exc):
            raise RuntimeError(
                f"Checkpoint модели «{model_id}» не может быть загружен безопасным "
                "загрузчиком torch: файл содержит объекты, выходящие за пределы "
                "разрешённых типов (weights_only=True).\n"
                f"Файл: {path}\nОшибка: {exc}\n"
                "Файл получен из стороннего источника и не будет загружен в "
                "небезопасном режиме."
            ) from exc
        raise RuntimeError(
            f"Не удалось прочитать checkpoint модели «{model_id}».\n"
            f"Файл: {path}\nОшибка: {exc}"
        ) from exc

    if not isinstance(checkpoint, dict):
        raise RuntimeError(
            f"Checkpoint модели «{model_id}» имеет неожиданный формат "
            f"({type(checkpoint).__name__}). Файл: {path}"
        )
    payload = checkpoint.get("state_dict", checkpoint)
    if not isinstance(payload, dict):
        raise RuntimeError(
            f"В checkpoint модели «{model_id}» поле 'state_dict' имеет неожиданный тип "
            f"({type(payload).__name__}). Файл: {path}"
        )
    return {_strip_module_prefix(key): value for key, value in payload.items()}


def _strip_module_prefix(key: str) -> str:
    """Drop a leading `module.` left behind by `nn.DataParallel`."""
    return key[len("module.") :] if key.startswith("module.") else key


def _load_into_network(net: Any, state_dict: dict[str, Any], model_id: str) -> None:
    """Strictly load `state_dict` into `net`.

    Strict on purpose: a missing or unexpected key means the constructor
    arguments no longer match the checkpoint (SLBR's `k_center` alone shifts the
    parameter count by 43 459), and a partially loaded network silently produces
    garbage.

    # Errors
    Raises `RuntimeError` with the mismatch reported by Torch.
    """
    try:
        net.load_state_dict(state_dict)
    except Exception as exc:
        raise RuntimeError(
            f"Веса модели «{model_id}» не соответствуют её архитектуре.\nОшибка: {exc}"
        ) from exc


def _is_restricted_unpickler_error(exc: BaseException) -> bool:
    """Whether `exc` is torch's `weights_only=True` refusal rather than a real I/O error."""
    text = str(exc)
    return "weights_only" in text or "WeightsUnpickler" in text or "Unsupported global" in text


# =====================================================================
#  Weight download (Google Drive)
# =====================================================================
def _download_from_google_drive(
    file_id: str, dest: Path, on_chunk: Callable[[int, int], None]
) -> None:
    """Stream the Drive file `file_id` into `dest`, handling the confirm interstitial.

    Files over Drive's 100 MB virus-scan threshold answer the first GET with an
    HTML form instead of the payload; the `confirm` token and the per-session
    `uuid` are parsed out of it and the request is repeated over the same
    `requests.Session`, which carries the cookie. Harmless for the smaller files.

    `on_chunk(done_bytes, expected_bytes)` is called per chunk.

    # Errors
    Raises `RuntimeError` on a transport error, on a missing `requests`, or when
    the second attempt is still an HTML interstitial.
    """
    try:
        import requests
    except Exception as exc:  # pragma: no cover - requests is in requirements.txt
        raise RuntimeError(
            "Для загрузки весов модели удаления водяных знаков требуется пакет requests."
        ) from exc

    url = _DRIVE_URL.format(file_id=file_id)
    try:
        with requests.Session() as session:
            with session.get(
                url, stream=True, allow_redirects=True, timeout=_DOWNLOAD_TIMEOUT_S
            ) as first:
                first.raise_for_status()
                if not _looks_like_html(first.headers.get("Content-Type", "")):
                    stream_response_to_file(first, dest, on_chunk)
                    return
                # Over the 100 MB virus-scan threshold: the payload is behind a
                # confirmation form. `session` carries the cookie it sets.
                confirm_url = _build_confirm_url(url, first.text)
            with session.get(
                confirm_url, stream=True, allow_redirects=True, timeout=_DOWNLOAD_TIMEOUT_S
            ) as second:
                second.raise_for_status()
                if _looks_like_html(second.headers.get("Content-Type", "")):
                    raise RuntimeError(
                        "Google Drive вернул HTML-страницу вместо файла весов. "
                        "Возможно, превышен лимит скачиваний или файл более недоступен."
                    )
                stream_response_to_file(second, dest, on_chunk)
    except RuntimeError:
        # Already carries an actionable message (interstitial, disk failure).
        raise
    except Exception as exc:
        # `requests`' own exceptions derive from `OSError`, so a disk error must
        # never be diagnosed here - `stream_response_to_file` owns that case.
        raise RuntimeError(
            f"Не удалось скачать веса модели с Google Drive (id {file_id}).\nОшибка: {exc}"
        ) from exc


def _build_confirm_url(base_url: str, html: str) -> str:
    """Append Drive's `confirm` token and per-session `uuid` parsed out of `html`.

    Falls back to `confirm=t` without a uuid when the form cannot be parsed:
    that is still the documented flow and merely fails one step later, with the
    HTML check as the backstop.
    """
    confirm = _first_group(r'name="confirm"\s+value="([^"]+)"', html) or "t"
    uuid = _first_group(r'name="uuid"\s+value="([^"]+)"', html)
    url = f"{base_url}&confirm={confirm}"
    if uuid:
        url = f"{url}&uuid={uuid}"
    return url


def _first_group(pattern: str, text: str) -> str | None:
    """First capture group of `pattern` in `text`, or `None`."""
    match = re.search(pattern, text)
    return match.group(1) if match else None


def _looks_like_html(content_type: str) -> bool:
    """Whether a Content-Type header announces an HTML document."""
    return "text/html" in content_type.lower()


def _validate_checkpoint_magic(path: Path, model_id: str) -> None:
    """Reject anything that is not a Torch checkpoint by its first bytes.

    This is the guard that catches a Google Drive HTML interstitial saved as a
    `.pth.tar`, and it runs regardless of whether a SHA-256 is known. Accepted:
    the modern zip container (`PK\\x03\\x04`) and a protocol>=2 pickle stream
    (`\\x80`), which is what all three legacy checkpoints are.

    # Errors
    Raises `RuntimeError` naming the offending file.
    """
    try:
        with path.open("rb") as handle:
            head = handle.read(8)
    except OSError as exc:
        raise RuntimeError(f"Не удалось прочитать файл весов: {path}\nОшибка: {exc}") from exc

    if head.startswith(_ZIP_MAGIC) or head.startswith(_PICKLE_PROTO_OPCODE):
        return

    lowered = head.lstrip().lower()
    hint = (
        "Скорее всего скачана HTML-страница Google Drive, а не файл весов."
        if lowered.startswith(b"<!doct") or lowered.startswith(b"<html")
        else "Файл повреждён или скачан не полностью."
    )
    raise RuntimeError(
        f"Файл весов модели «{model_id}» не является checkpoint'ом torch. {hint}\n"
        f"Файл: {path}\nПервые байты: {head!r}"
    )


def _verify_optional_sha256(path: Path, model_id: str) -> None:
    """Compare `path` against the pinned digest when one is known.

    A `None` entry in `_WEIGHT_SHA256` means the digest has not been captured
    yet; the file is then accepted on the magic-byte gate alone and the fact is
    logged, so an unverified weight is never silently indistinguishable from a
    verified one.

    # Errors
    Raises `RuntimeError` on a mismatch.
    """
    import hashlib

    expected = _WEIGHT_SHA256.get(model_id)
    if expected is None:
        log.info(
            "watermark: checkpoint %r accepted without a SHA-256 comparison "
            "(no digest recorded for this file yet); magic-byte check passed",
            model_id,
        )
        return

    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    actual = digest.hexdigest()
    if actual != expected:
        raise RuntimeError(
            f"Проверка целостности весов модели «{model_id}» не пройдена.\n"
            f"Файл: {path}\nОжидалось: {expected}\nПолучено:  {actual}"
        )


# =====================================================================
#  Geometry / image helpers
# =====================================================================
def _plan_tiles(height: int, width: int, tile: int, overlap: int) -> list[tuple[int, int]]:
    """Top-left offsets of the square tiles covering a `height` x `width` area.

    `height` and `width` must already be at least `tile`. The last row/column is
    flushed against the far edge, so the final overlap may exceed `overlap` but
    no pixel is ever left uncovered.
    """
    return [(top, left) for top in _axis_offsets(height, tile, overlap)
            for left in _axis_offsets(width, tile, overlap)]


def _axis_offsets(extent: int, tile: int, overlap: int) -> list[int]:
    """Window start offsets covering `[0, extent)` along one axis."""
    if extent <= tile:
        return [0]
    stride = max(1, tile - overlap)
    offsets = list(range(0, extent - tile + 1, stride))
    if offsets[-1] != extent - tile:
        offsets.append(extent - tile)
    return offsets


def _feather_window(tile: int, overlap: int) -> np.ndarray:
    """1-D raised-cosine blend window of length `tile` with `overlap`-px ramps.

    The ramp is `0.5 - 0.5*cos(pi * (i + 0.5) / overlap)`, whose head and
    reversed tail sum to exactly 1 — so two tiles spaced `tile - overlap` apart
    form a partition of unity across their shared band. Border tiles have no
    partner, which is why the caller still normalizes by the accumulated weight.
    """
    import numpy as np

    window = np.ones(tile, dtype=np.float32)
    if overlap > 0:
        ramp = 0.5 - 0.5 * np.cos(np.pi * (np.arange(overlap, dtype=np.float32) + 0.5) / overlap)
        window[:overlap] = ramp
        window[tile - overlap :] = ramp[::-1]
    return window


def _downscale_long_side(rgb: np.ndarray, long_side: int) -> np.ndarray:
    """Scale `rgb` down so its longer side is `long_side`. Never upscales."""
    import numpy as np
    from PIL import Image

    height, width = rgb.shape[:2]
    longest = max(height, width)
    if longest <= long_side or longest == 0:
        return rgb
    scale = long_side / float(longest)
    new_w = max(1, int(round(width * scale)))
    new_h = max(1, int(round(height * scale)))
    resized = Image.fromarray(rgb, "RGB").resize((new_w, new_h), Image.LANCZOS)
    return np.ascontiguousarray(np.asarray(resized, dtype=np.uint8))


def _pad_square_multiple(rgb: np.ndarray, multiple: int) -> tuple[np.ndarray, tuple[int, int]]:
    """Reflect-pad `rgb` to a square whose side is a multiple of `multiple`.

    Returns `(padded, (original_h, original_w))`. One policy for all three
    models: SLBR needs square AND `% 16 == 0`, SplitNet needs `% 16 == 0`, WDNet
    needs nothing — padding all of them costs a little wasted compute and removes
    every special case.
    """
    height, width = rgb.shape[:2]
    side = max(height, width, multiple)
    if multiple > 1:
        side = ((side + multiple - 1) // multiple) * multiple
    padded, original = _pad_reflect(rgb, side, side)
    return padded, original


def _pad_reflect(rgb: np.ndarray, target_h: int, target_w: int) -> tuple[np.ndarray, tuple[int, int]]:
    """Reflect-pad `rgb` at the bottom/right up to `(target_h, target_w)`.

    Returns `(padded, (original_h, original_w))`. A target smaller than the
    input is treated as "no padding on that axis" rather than a crop.
    """
    import numpy as np

    height, width = rgb.shape[:2]
    pad_h = max(0, target_h - height)
    pad_w = max(0, target_w - width)
    if pad_h == 0 and pad_w == 0:
        return rgb, (height, width)
    pad_width = [(0, pad_h), (0, pad_w)] + [(0, 0)] * (rgb.ndim - 2)
    return np.ascontiguousarray(np.pad(rgb, pad_width, mode="reflect")), (height, width)


def _resize_mask_float(mask: np.ndarray, width: int, height: int) -> np.ndarray:
    """Bilinearly resize a float32 `[0, 1]` mask to `width` x `height`."""
    import numpy as np
    from PIL import Image

    if mask.shape[0] == height and mask.shape[1] == width:
        return np.ascontiguousarray(mask.astype(np.float32))
    as_u8 = np.clip(np.round(mask * 255.0), 0, 255).astype(np.uint8)
    resized = Image.fromarray(as_u8, "L").resize((width, height), Image.BILINEAR)
    return np.ascontiguousarray(np.asarray(resized, dtype=np.float32) / 255.0)


def _binarize_and_dilate(mask: np.ndarray, threshold: float, dilate_px: int) -> np.ndarray:
    """Threshold a float `[0, 1]` mask into L8 `{0, 255}` and dilate it by `dilate_px`."""
    import numpy as np

    binary = np.where(mask >= float(threshold), 255, 0).astype(np.uint8)
    if dilate_px <= 0:
        return np.ascontiguousarray(binary)
    return np.ascontiguousarray(_dilate_mask(binary, int(dilate_px)))


def _dilate_mask(mask: np.ndarray, dilate: int) -> np.ndarray:
    """Dilate an L8 binary mask by `dilate` px, via cv2 when available."""
    import numpy as np

    try:
        import cv2

        size = 2 * dilate + 1
        kernel = cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (size, size))
        return cv2.dilate(mask, kernel, iterations=1)
    except Exception:
        from PIL import Image, ImageFilter

        # PIL's MaxFilter caps at 31 px, so a larger radius is applied in passes.
        image = Image.fromarray(mask, "L")
        remaining = dilate
        while remaining > 0:
            step = min(remaining, 15)
            image = image.filter(ImageFilter.MaxFilter(2 * step + 1))
            remaining -= step
        return np.asarray(image, dtype=np.uint8)


def _decode_image_rgb(image_bytes: bytes) -> np.ndarray:
    """Decode PNG/JPEG bytes into a contiguous RGB uint8 array.

    # Errors
    Raises `ValueError` when the payload is not a decodable image.
    """
    import numpy as np
    from PIL import Image, UnidentifiedImageError

    try:
        with Image.open(io.BytesIO(image_bytes)) as image:
            return np.ascontiguousarray(np.array(image.convert("RGB"), dtype=np.uint8))
    except (UnidentifiedImageError, OSError, ValueError) as exc:
        raise ValueError(f"Не удалось декодировать изображение: {exc}") from exc


def _encode_png_bytes_rgb(image_rgb: np.ndarray) -> bytes:
    """Encode an RGB uint8 array as PNG bytes."""
    import numpy as np
    from PIL import Image

    array = np.ascontiguousarray(image_rgb.astype(np.uint8))
    with io.BytesIO() as buffer:
        Image.fromarray(array, "RGB").save(buffer, format="PNG")
        return buffer.getvalue()


def _encode_png_bytes_l8(mask: np.ndarray) -> bytes:
    """Encode a single-channel uint8 mask as an L8 PNG."""
    import numpy as np
    from PIL import Image

    array = np.ascontiguousarray(mask.astype(np.uint8))
    with io.BytesIO() as buffer:
        Image.fromarray(array, "L").save(buffer, format="PNG")
        return buffer.getvalue()


def _emit(callback: ProgressCb | None, phase: str, step: int, total: int, label: str) -> None:
    """Call `callback` defensively — a broken progress sink must not abort the job."""
    if callback is None:
        return
    try:
        callback(phase, int(step), int(max(total, 1)), label)
    except Exception:
        log.debug("watermark: progress callback raised", exc_info=True)


def _clear_torch_cache() -> None:
    """Collect garbage and free the accelerator allocator cache when torch is present."""
    import gc

    gc.collect()
    try:
        import torch

        if hasattr(torch, "cuda") and torch.cuda.is_available():
            torch.cuda.empty_cache()
            if hasattr(torch.cuda, "ipc_collect"):
                torch.cuda.ipc_collect()
    except Exception:
        log.debug("watermark: could not clear the torch allocator cache", exc_info=True)


# =====================================================================
#  Device selection
# =====================================================================
def _resolve_selected_backend_device(fallback: str) -> str:
    """Resolve `General.ai_device` to a device string usable right now.

    `not-selected` (the config sentinel) and any unavailable choice fall back to
    an available accelerator and finally to `cpu`. Unlike `flux_fill.py` this
    service does NOT pin itself to a discrete GPU: the three networks are small
    (1.9-4.8 GB at 1024²) and must remain usable on CPU.
    """
    fallback_norm = _normalize_backend_device(fallback, "cpu")
    configured = _read_configured_device()
    if configured is None:
        configured = fallback_norm

    normalized = _normalize_backend_device(configured, fallback_norm)
    available = _safe_available_devices()

    if normalized in available:
        return normalized
    if normalized.startswith("cuda") and "cuda" in available:
        return "cuda"
    if fallback_norm in available:
        return fallback_norm
    if "cuda" in available:
        return "cuda"
    if "mps" in available:
        return "mps"
    return "cpu"


def _read_configured_device() -> str | None:
    """`General.ai_device` from the user config, or `None` for the sentinel."""
    config_root = getattr(UserConfig, "config", None)
    if not isinstance(config_root, dict):
        return None
    general = config_root.get("General")
    if not isinstance(general, dict):
        return None
    value = general.get("ai_device")
    if not isinstance(value, str):
        return None
    value = value.strip().lower()
    if value == "not-selected":
        return None
    return value or None


def _safe_available_devices() -> set[str]:
    """Devices Torch can actually use right now; `{"cpu"}` if probing fails."""
    try:
        return set(AIDevice.detect_available_devices())
    except Exception:
        log.debug("watermark: device probing failed, assuming CPU only", exc_info=True)
        return {"cpu"}


def _normalize_backend_device(raw: str, fallback: str) -> str:
    """Coerce a device string to `cpu` / `mps` / `cuda` / `cuda:<n>`."""
    value = str(raw or "").strip().lower()
    if value in {"cpu", "mps", "cuda"}:
        return value
    if value.startswith("cuda:"):
        return value
    return str(fallback or "cpu").strip().lower() or "cpu"


# =====================================================================
#  Coercion helpers
# =====================================================================
def _to_int(value: Any, default: int) -> int:
    """Best-effort int coercion; `bool` and unparsable values yield `default`."""
    if isinstance(value, bool):
        return default
    try:
        return int(value)
    except (TypeError, ValueError):
        return default


def _clamp_int(value: Any, *, default: int, low: int, high: int) -> int:
    """`_to_int` clamped to `[low, high]`."""
    return max(low, min(high, _to_int(value, default)))


def _clamp_float(value: Any, *, default: float, low: float, high: float) -> float:
    """Best-effort float coercion clamped to `[low, high]`."""
    if isinstance(value, bool):
        out = default
    else:
        try:
            out = float(value)
        except (TypeError, ValueError):
            out = default
    if out != out:  # NaN: no sane clamp, fall back to the documented default.
        out = default
    return max(low, min(high, out))
