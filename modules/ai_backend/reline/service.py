"""
File: modules/ai_backend/reline/service.py

Purpose:
Reline service adapter for the Python AI backend.

Main responsibilities:
- resolve local or catalog-backed Reline super-resolution model files;
- download direct model files through the shared retrying transfer
  (`engines.model_download.download_http_to_path`) or extract model checkpoints
  from tar.xz archives;
- build and run a Reline pipeline for one image file;
- expose a compact model catalog payload for Rust UI helpers.

Notes:
The Python `reline` package does not own model downloads. This adapter keeps Reline
checkpoint files under `ManhwaStudio_AI_Models/side_models/Reline` and passes the resolved
local checkpoint path into the standard Reline `upscale` node.

The models are user-chosen from a live catalog, so nothing is pinned: the expected size of a
download is the server's `Content-Length` (`expected_size=0`), which still lets a short body be
detected and resumed with `Range`.

Besides the remote catalog (`CATALOG_URL`), a small built-in `EXTRA_MODELS` list exposes models
that the remote catalog does not yet publish. Entries with an empty `url` (e.g. only a Google
Drive folder exists) resolve from a manually placed local checkpoint and otherwise raise a clear
download hint pointing at `source`.
"""

from __future__ import annotations

import json
import logging
import lzma
import shutil
import tarfile
import tempfile
from pathlib import Path
from typing import Any, Callable
from urllib.parse import urlparse

from ..engines.model_download import (
    DownloadError,
    GetFn,
    download_http_to_path,
    fetch_small_body,
)
from ..runtime.paths import side_models_root

log = logging.getLogger(__name__)

CATALOG_URL = "https://mdb.yor.ovh/v1/files"
# Module attribute (not a function) on purpose: the tests patch it. The root comes from the
# backend's single owner of the side-model root, `runtime.paths.side_models_root()`.
MODEL_DIR = side_models_root() / "Reline"
DOWNLOAD_DIR = MODEL_DIR / ".download"
MODEL_SUFFIXES = (".pt", ".pth", ".ckpt", ".safetensors")
ARCHIVE_SUFFIXES = (".tar.xz", ".txz")
#: Sent with the catalog request and every model GET; the catalog host has served
#: this agent since the adapter was written.
USER_AGENT = "ManhwaStudio/Reline"

# Built-in models not (yet) present in the remote catalog. Each entry is resolved from a local
# checkpoint placed in MODEL_DIR. `url` may be empty when only a non-direct source exists (e.g. a
# Google Drive folder); in that case the checkpoint must be downloaded and placed manually, and
# resolution raises a clear error pointing at `source`.
EXTRA_MODELS: tuple[dict[str, str], ...] = (
    {
        "name": "2x_enhancr_da_smosr",
        "filename": "2x_enhancr_da_smosr.pth",
        "url": "",
        "source": "https://drive.google.com/drive/u/2/folders/1dNMkUd4V8cnXAURHVPtAmWj0j5OZF8fV",
    },
)

READER_MODES = {"rgb", "gray", "dynamic"}
TILERS = {"exact", "max", "no_tiling"}
DTYPES = {"F32", "F16", "BF16"}
RESIZE_FILTERS = {
    "nearest",
    "box",
    "sbox4",
    "sbox8",
    "ibox",
    "linear",
    "slinear4",
    "slinear8",
    "ilinear",
    "hamming",
    "shamming4",
    "shamming8",
    "ihamming",
    "catmullrom",
    "scatmullrom4",
    "scatmullrom8",
    "icatmullrom",
    "mitchell",
    "smitchell4",
    "smitchell8",
    "imitchell",
    "lanczos",
    "slanczos4",
    "slanczos8",
    "ilanczos",
    "gauss",
    "sgauss4",
    "sgauss8",
    "igauss",
    "dpid_0.25",
    "dpid_0.5",
    "dpid_0.75",
    "dpid_1",
}
HALFTONE_FILTERS = {
    "nearest",
    "box",
    "sbox4",
    "sbox8",
    "linear",
    "slinear4",
    "slinear8",
    "hamming",
    "shamming4",
    "shamming8",
    "catmullrom",
    "scatmullrom4",
    "scatmullrom8",
    "mitchell",
    "smitchell4",
    "smitchell8",
    "lanczos",
    "slanczos4",
    "slanczos8",
    "gauss",
    "sgauss4",
    "sgauss8",
}
DOT_TYPES = {"line", "cross", "ellipse", "invline", "circle"}
HALFTONE_MODES = {"gray", "rgb", "hsv", "cmyk"}
CANNY_TYPES = {"invert", "normal", "unsharp"}
CVT_TYPES = {"RGB2Gray2020", "RGB2Gray709", "RGB2Gray", "Gray2RGB"}


class RelineService:
    """`reline.models` / `reline.process` adapter.

    Construction has no side effect: `MODEL_DIR` and `DOWNLOAD_DIR` are created only when a
    model is actually downloaded, so building the service (the backend's `AppState`, tests)
    never writes into the model tree.
    """

    def health(self) -> dict[str, Any]:
        return {
            "ready": True,
            "model_dir": str(MODEL_DIR),
        }

    def list_models(self, *, should_cancel: Callable[[], bool] | None = None) -> list[dict[str, Any]]:
        """The remote catalog plus `EXTRA_MODELS`, each marked with whether it is on disk.

        # Errors
        `RuntimeError` when the catalog cannot be fetched or parsed; `DownloadCancelled` when
        `should_cancel` stopped the fetch.
        """
        catalog = _fetch_catalog(should_cancel=should_cancel)
        models: list[dict[str, Any]] = []
        seen: set[str] = set()
        for entry in catalog:
            filename = _entry_filename(entry)
            if not filename:
                continue
            name = _model_storage_stem(filename)
            models.append(
                {
                    "filename": filename,
                    "name": name,
                    "url": str(entry.get("url", "")).strip(),
                    "downloaded": _find_existing_model(filename) is not None,
                }
            )
            seen.add(_model_lookup_key(name))
        # Append built-in models that the remote catalog does not (yet) expose.
        for extra in EXTRA_MODELS:
            if _model_lookup_key(extra["name"]) in seen:
                continue
            models.append(
                {
                    "filename": extra["filename"],
                    "name": extra["name"],
                    "url": str(extra.get("url", "")).strip(),
                    "downloaded": _find_existing_model(extra["filename"]) is not None,
                }
            )
        return models

    def process_image_file(
        self,
        *,
        image_path: str,
        output_path: str | None,
        params: dict[str, Any],
        should_cancel: Callable[[], bool] | None = None,
    ) -> dict[str, Any]:
        """Run the Reline pipeline `params` describes on `image_path`.

        `should_cancel` is polled by a model download or catalog fetch the upscale node needs
        (per chunk and during retry waits); the pipeline itself is not interruptible.

        # Errors
        `FileNotFoundError` for a missing input or model, `ValueError` for an invalid option,
        `RuntimeError` when a download or the pipeline fails, `DownloadCancelled` when
        `should_cancel` stopped a download.
        """
        input_path = Path(image_path).expanduser()
        if not input_path.is_file():
            raise FileNotFoundError(f"Reline input image not found: {input_path}")

        if output_path:
            result_path = Path(output_path).expanduser()
            result_path.parent.mkdir(parents=True, exist_ok=True)
            cleanup_dir: tempfile.TemporaryDirectory[str] | None = None
        else:
            cleanup_dir = tempfile.TemporaryDirectory(prefix="mf_reline_output_")
            result_path = Path(cleanup_dir.name) / "output.png"

        try:
            pipeline_json = self._build_pipeline_json(input_path, result_path, params, should_cancel)
            log.info(
                "reline: process start input=%s output=%s nodes=%d",
                input_path,
                result_path,
                len(pipeline_json),
            )
            # Absolute import of the THIRD-PARTY top-level `reline` package, not
            # this sub-package (`modules.ai_backend.reline`): Python 3 imports are
            # absolute, so the identical names never collide. Imported lazily so
            # the Torch-backed pipeline loads only when a job actually runs.
            from reline import Pipeline

            Pipeline.from_json(pipeline_json).process_linear(with_tqdm=False)
            if not result_path.is_file():
                raise RuntimeError(f"Reline did not create output file: {result_path}")

            return {
                "ok": True,
                "engine": "reline",
                "output_path": str(result_path),
                "pipeline": pipeline_json,
            }
        except Exception:
            log.exception("reline: processing failed for input=%s", input_path)
            raise
        finally:
            if cleanup_dir is not None:
                cleanup_dir.cleanup()

    def _build_pipeline_json(
        self,
        input_path: Path,
        result_path: Path,
        params: dict[str, Any],
        should_cancel: Callable[[], bool] | None = None,
    ) -> list[dict[str, Any]]:
        reader_mode = _choice(params.get("reader_mode", "rgb"), READER_MODES, "reader_mode")
        nodes: list[dict[str, Any]] = [
            {
                "type": "file_reader",
                "options": {
                    "path": str(input_path),
                    "mode": reader_mode,
                },
            }
        ]

        upscale = _object(params.get("upscale"))
        if _bool(upscale.get("enabled"), False):
            model_path = _resolve_model(upscale, should_cancel=should_cancel)
            nodes.append(
                {
                    "type": "upscale",
                    "options": {
                        "model": str(model_path),
                        "tiler": _choice(upscale.get("tiler", "exact"), TILERS, "upscale.tiler"),
                        "target_scale": _optional_positive_int(
                            upscale.get("target_scale"), "upscale.target_scale"
                        ),
                        "dtype": _choice(upscale.get("dtype", "F32"), DTYPES, "upscale.dtype"),
                        "exact_tiler_size": _positive_int(
                            upscale.get("exact_tiler_size", 800), "upscale.exact_tiler_size"
                        ),
                        "allow_cpu_upscale": _bool(
                            upscale.get("allow_cpu_upscale"), False
                        ),
                    },
                }
            )

        sharp = _object(params.get("sharp"))
        if _bool(sharp.get("enabled"), False):
            nodes.append(
                {
                    "type": "sharp",
                    "options": {
                        "low_input": _int(sharp.get("low_input", 0), "sharp.low_input"),
                        "high_input": _int(sharp.get("high_input", 255), "sharp.high_input"),
                        "gamma": _float(sharp.get("gamma", 1.0), "sharp.gamma"),
                        "diapason_white": _int(
                            sharp.get("diapason_white", -1), "sharp.diapason_white"
                        ),
                        "diapason_black": _int(
                            sharp.get("diapason_black", -1), "sharp.diapason_black"
                        ),
                        "canny": _bool(sharp.get("canny"), False),
                        "canny_type": _choice(
                            sharp.get("canny_type", "normal"), CANNY_TYPES, "sharp.canny_type"
                        ),
                    },
                }
            )

        halftone = _object(params.get("halftone"))
        if _bool(halftone.get("enabled"), False):
            nodes.append(
                {
                    "type": "halftone",
                    "options": {
                        "dot_size": _int_or_int_list(halftone.get("dot_size", 7), "halftone.dot_size"),
                        "angle": _int_or_int_list(halftone.get("angle", 0), "halftone.angle"),
                        "dot_type": _choice_or_choice_list(
                            halftone.get("dot_type", "circle"),
                            DOT_TYPES,
                            "halftone.dot_type",
                        ),
                        "halftone_mode": _choice(
                            halftone.get("halftone_mode", "gray"),
                            HALFTONE_MODES,
                            "halftone.halftone_mode",
                        ),
                        "ssaa_scale": _optional_float(
                            halftone.get("ssaa_scale"), "halftone.ssaa_scale"
                        ),
                        "ssaa_filter": _choice(
                            halftone.get("ssaa_filter", "shamming4"),
                            HALFTONE_FILTERS,
                            "halftone.ssaa_filter",
                        ),
                        "disable_auto_dot": _bool(
                            halftone.get("disable_auto_dot"), False
                        ),
                    },
                }
            )

        resize = _object(params.get("resize"))
        if _bool(resize.get("enabled"), False):
            resize_options = {
                "height": _optional_positive_int(resize.get("height"), "resize.height"),
                "width": _optional_positive_int(resize.get("width"), "resize.width"),
                "percent": _optional_positive_float(resize.get("percent"), "resize.percent"),
                "filter": _choice(resize.get("filter", "catmullrom"), RESIZE_FILTERS, "resize.filter"),
                "gamma_correction": _bool(resize.get("gamma_correction"), False),
                "spread": _bool(resize.get("spread"), False),
                "spread_size": _positive_int(resize.get("spread_size", 2800), "resize.spread_size"),
            }
            if (
                resize_options["height"] is None
                and resize_options["width"] is None
                and resize_options["percent"] is None
            ):
                raise ValueError("Reline resize requires height, width, or percent.")
            nodes.append({"type": "resize", "options": resize_options})

        level = _object(params.get("level"))
        if _bool(level.get("enabled"), False):
            nodes.append(
                {
                    "type": "level",
                    "options": {
                        "low_input": _int(level.get("low_input", 0), "level.low_input"),
                        "high_input": _int(level.get("high_input", 255), "level.high_input"),
                        "low_output": _int(level.get("low_output", 0), "level.low_output"),
                        "high_output": _int(level.get("high_output", 255), "level.high_output"),
                        "gamma": _float(level.get("gamma", 1.0), "level.gamma"),
                    },
                }
            )

        cvt_color = _object(params.get("cvt_color"))
        if _bool(cvt_color.get("enabled"), False):
            nodes.append(
                {
                    "type": "cvt_color",
                    "options": {
                        "cvt_type": _choice(
                            cvt_color.get("cvt_type", "RGB2Gray2020"),
                            CVT_TYPES,
                            "cvt_color.cvt_type",
                        ),
                    },
                }
            )

        nodes.append(
            {
                "type": "file_writer",
                "options": {
                    "path": str(result_path),
                },
            }
        )
        return nodes


def _resolve_model(
    options: dict[str, Any], *, should_cancel: Callable[[], bool] | None = None
) -> Path:
    """Local checkpoint path for the `upscale` options, downloading the model when needed.

    `should_cancel` reaches the catalog fetch and the download.
    """
    model_path_raw = str(options.get("model_path", "") or "").strip()
    if model_path_raw:
        path = Path(model_path_raw).expanduser()
        if not path.is_file():
            raise FileNotFoundError(f"Reline model file not found: {path}")
        if path.suffix.lower() not in MODEL_SUFFIXES:
            raise ValueError(f"Reline model has unsupported suffix: {path}")
        return path

    model_name = str(options.get("model_name", "") or options.get("model", "") or "").strip()
    model_url = str(options.get("model_url", "") or "").strip()
    if not model_name and model_url:
        model_name = _filename_from_url(model_url)
    if not model_name:
        raise ValueError("Reline upscale requires model_name, model_url, or model_path.")

    existing = _find_existing_model(model_name)
    if existing is not None:
        return existing

    catalog_entry: dict[str, Any] | None = None
    if not model_url:
        extra = _find_extra_model(model_name)
        if extra is not None:
            model_url = str(extra.get("url", "") or "").strip()
            if not model_url:
                source = str(extra.get("source", "") or "").strip()
                hint = (
                    f" Download it from {source} and place it into {MODEL_DIR}."
                    if source
                    else f" Place the checkpoint into {MODEL_DIR}."
                )
                raise FileNotFoundError(
                    f"Reline model '{model_name}' is a built-in model without a direct "
                    f"download URL.{hint}"
                )
        else:
            catalog_entry = _find_catalog_entry(model_name, should_cancel=should_cancel)
            if catalog_entry is None:
                raise FileNotFoundError(
                    f"Reline model '{model_name}' was not found locally or in the model catalog."
                )
            model_url = str(catalog_entry.get("url", "") or "").strip()

    if not model_url:
        raise ValueError(f"Reline model '{model_name}' does not have a download URL.")

    if catalog_entry is not None:
        filename = _entry_filename(catalog_entry)
    elif _has_supported_download_suffix(model_name):
        filename = _safe_filename(model_name)
    else:
        filename = _filename_from_url(model_url)
    return _download_model(model_url, filename, should_cancel=should_cancel)


def _find_extra_model(model_name: str) -> dict[str, str] | None:
    """Return the built-in EXTRA_MODELS entry matching `model_name` by name or filename."""
    wanted = _model_lookup_key(model_name)
    for extra in EXTRA_MODELS:
        candidates = (extra.get("name", ""), extra.get("filename", ""))
        if any(value and _model_lookup_key(value) == wanted for value in candidates):
            return extra
    return None


def _find_catalog_entry(
    model_name: str, *, should_cancel: Callable[[], bool] | None = None
) -> dict[str, Any] | None:
    """The catalog entry matching `model_name` by filename, name or URL, or `None`."""
    wanted = _model_lookup_key(model_name)
    for entry in _fetch_catalog(should_cancel=should_cancel):
        candidate_values = [
            _entry_filename(entry),
            str(entry.get("name", "") or "").strip(),
            str(entry.get("url", "") or "").strip(),
        ]
        if any(_model_lookup_key(value) == wanted for value in candidate_values if value):
            return entry
    return None


def _fetch_catalog(
    *,
    should_cancel: Callable[[], bool] | None = None,
    get: GetFn | None = None,
    sleep: Callable[[float], None] | None = None,
) -> list[dict[str, Any]]:
    """Fetch the remote model catalog (`CATALOG_URL`) as a list of entry objects.

    Rides `fetch_small_body`, so it has the shared network rules: connect/read timeouts,
    transient failures retried with backoff, cancel polled per chunk and during waits. `get` /
    `sleep` are the transport and clock seams (`None`: `requests.get` / `time.sleep`).

    # Errors
    `RuntimeError` for a permanent HTTP status, exhausted retries, an oversized body or a
    response that is not a JSON array; `DownloadCancelled` when `should_cancel` stopped it.
    """
    import requests

    try:
        body = fetch_small_body(
            CATALOG_URL,
            label="каталог моделей Reline",
            headers={"User-Agent": USER_AGENT},
            should_cancel=should_cancel,
            get=get,
            sleep=sleep,
        )
        payload = json.loads(body.decode("utf-8"))
    except (requests.HTTPError, DownloadError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        log.error("reline: could not fetch the model catalog from %s: %s", CATALOG_URL, exc)
        raise RuntimeError(f"Could not fetch Reline model catalog from {CATALOG_URL}: {exc}") from exc

    if not isinstance(payload, list):
        raise RuntimeError("Reline model catalog response is not a JSON array.")
    return [entry for entry in payload if isinstance(entry, dict)]


def _download_model(
    url: str,
    filename: str,
    *,
    should_cancel: Callable[[], bool] | None = None,
    get: GetFn | None = None,
    sleep: Callable[[float], None] | None = None,
) -> Path:
    """Download a Reline model from `url` and return the local checkpoint path.

    The transfer is `download_http_to_path` into `DOWNLOAD_DIR/<filename>`: transient network
    failures are retried with backoff and resumed with `Range`, a failed run keeps its bytes for
    the next one, and the file appears only complete. The expected size is the server's
    `Content-Length` (catalog models are not pinned). An archive is then extracted into
    `MODEL_DIR`; a plain checkpoint is moved there. `should_cancel` is polled per chunk and
    during retry waits (a cancel parks the bytes for a resume). `get` / `sleep` are the
    transport and clock seams (`None`: `requests.get` / `time.sleep`).

    # Errors
    `ValueError` for a non-http(s) URL or a filename without a supported model/archive suffix
    (refused before any request); `RuntimeError` for a permanent HTTP status, exhausted retries,
    a staging write failure, or an archive that cannot be extracted; `DownloadCancelled` when
    `should_cancel` stopped the transfer.
    """
    parsed = urlparse(url)
    if parsed.scheme not in {"http", "https"}:
        raise ValueError(f"Reline model URL must be http or https: {url}")

    safe_filename = _safe_filename(filename)
    if not _has_supported_download_suffix(safe_filename):
        raise ValueError(f"Reline model download has unsupported suffix: {safe_filename}")
    final_candidate = _find_existing_model(safe_filename)
    if final_candidate is not None:
        return final_candidate

    import requests

    archive_or_model_path = DOWNLOAD_DIR / safe_filename
    log.info("reline: downloading model %s from %s", safe_filename, url)
    try:
        download_http_to_path(
            url,
            archive_or_model_path,
            expected_size=0,
            headers={"User-Agent": USER_AGENT},
            # `reline.process` has no progress channel; retries are logged by the transfer.
            on_progress=lambda _done, _total: None,
            should_cancel=should_cancel,
            get=get,
            sleep=sleep,
        )
    except requests.HTTPError as exc:
        log.error("reline: model request refused for %s: %s", url, exc)
        raise RuntimeError(f"Could not download Reline model from {url}: {exc}") from exc

    if _has_archive_suffix(archive_or_model_path.name):
        return _extract_first_model_from_archive(archive_or_model_path)

    target_path = MODEL_DIR / archive_or_model_path.name
    _replace_file(archive_or_model_path, target_path)
    return target_path


def _extract_first_model_from_archive(archive_path: Path) -> Path:
    """Extract the first supported checkpoint of `archive_path` into `MODEL_DIR`.

    The checkpoint is stored under the archive's stem (`<stem><suffix>`) so a later
    catalog-name lookup finds it, and the archive is removed. An archive that cannot be read
    or holds no supported checkpoint is proven wrong and is removed too, so the next request
    downloads it again instead of re-extracting the same bad bytes.

    # Errors
    `RuntimeError` for an unreadable archive or one without a supported checkpoint.
    """
    MODEL_DIR.mkdir(parents=True, exist_ok=True)
    try:
        with tarfile.open(archive_path, mode="r:xz") as archive:
            for member in archive.getmembers():
                if not member.isfile():
                    continue
                member_name = Path(member.name).name
                if Path(member_name).suffix.lower() not in MODEL_SUFFIXES:
                    continue
                storage_stem = _model_storage_stem(archive_path.name)
                if storage_stem:
                    target_path = MODEL_DIR / _safe_filename(
                        f"{storage_stem}{Path(member_name).suffix.lower()}"
                    )
                else:
                    target_path = MODEL_DIR / _safe_filename(member_name)
                source = archive.extractfile(member)
                if source is None:
                    continue
                with source, target_path.with_suffix(target_path.suffix + ".part").open("wb") as out:
                    shutil.copyfileobj(source, out)
                target_path.with_suffix(target_path.suffix + ".part").replace(target_path)
                archive_path.unlink(missing_ok=True)
                return target_path
    except (tarfile.TarError, lzma.LZMAError, EOFError) as exc:
        # A truncated or corrupt xz stream surfaces as `LZMAError` / `EOFError`, not `TarError`.
        log.error("reline: could not extract %s: %s; removing it", archive_path, exc)
        archive_path.unlink(missing_ok=True)
        raise RuntimeError(f"Could not extract Reline model archive {archive_path}: {exc}") from exc
    log.error("reline: %s holds no supported checkpoint; removing it", archive_path)
    archive_path.unlink(missing_ok=True)
    raise RuntimeError(f"Reline archive does not contain a supported model file: {archive_path}")


def _find_existing_model(model_name: str) -> Path | None:
    """The checkpoint under `MODEL_DIR` matching `model_name`, or `None` (also when the
    directory does not exist yet: nothing was downloaded)."""
    safe_name = _safe_filename(model_name)
    direct = MODEL_DIR / safe_name
    if direct.is_file() and direct.suffix.lower() in MODEL_SUFFIXES:
        return direct
    if not MODEL_DIR.is_dir():
        return None

    wanted_key = _model_lookup_key(safe_name)
    for path in MODEL_DIR.iterdir():
        if not path.is_file() or path.suffix.lower() not in MODEL_SUFFIXES:
            continue
        if path.name.lower() == safe_name.lower() or _model_lookup_key(path.name) == wanted_key:
            return path
    return None


def _entry_filename(entry: dict[str, Any] | None) -> str:
    if not entry:
        return ""
    filename = str(entry.get("filename", "") or "").strip()
    if filename:
        return _safe_filename(filename)
    url = str(entry.get("url", "") or "").strip()
    if url:
        return _filename_from_url(url)
    name = str(entry.get("name", "") or "").strip()
    if name:
        return _safe_filename(name)
    return ""


def _filename_from_url(url: str) -> str:
    parsed = urlparse(url)
    name = Path(parsed.path).name
    if not name:
        raise ValueError(f"Could not derive model filename from URL: {url}")
    return _safe_filename(name)


def _safe_filename(filename: str) -> str:
    name = Path(str(filename).replace("\\", "/")).name.strip()
    if not name or name in {".", ".."}:
        raise ValueError(f"Invalid Reline model filename: {filename!r}")
    return name


def _has_archive_suffix(name: str) -> bool:
    lowered = name.lower()
    return any(lowered.endswith(suffix) for suffix in ARCHIVE_SUFFIXES)


def _has_supported_download_suffix(name: str) -> bool:
    lowered = name.lower()
    return lowered.endswith(MODEL_SUFFIXES) or _has_archive_suffix(lowered)


def _model_lookup_key(name_or_url: str) -> str:
    return _model_storage_stem(name_or_url).lower()


def _model_storage_stem(name_or_url: str) -> str:
    text = str(name_or_url).strip()
    if not text:
        return ""

    parsed = urlparse(text)
    if parsed.scheme and parsed.path:
        text = Path(parsed.path).name
    else:
        text = Path(text.replace("\\", "/")).name

    lowered = text.lower()
    for suffix in ARCHIVE_SUFFIXES:
        if lowered.endswith(suffix):
            return text[: -len(suffix)]
    for suffix in MODEL_SUFFIXES:
        if lowered.endswith(suffix):
            return text[: -len(suffix)]
    return Path(text).stem


def _replace_file(source: Path, target: Path) -> None:
    target.parent.mkdir(parents=True, exist_ok=True)
    partial = target.with_suffix(target.suffix + ".part")
    if partial.exists():
        partial.unlink()
    source.replace(partial)
    partial.replace(target)


def _object(value: Any) -> dict[str, Any]:
    return value if isinstance(value, dict) else {}


def _bool(value: Any, default: bool) -> bool:
    if value is None:
        return default
    if isinstance(value, bool):
        return value
    if isinstance(value, str):
        return value.strip().lower() in {"1", "true", "yes", "on"}
    return bool(value)


def _choice(value: Any, allowed: set[str], field_name: str) -> str:
    text = str(value or "").strip()
    if text not in allowed:
        raise ValueError(f"Invalid {field_name}: {text!r}")
    return text


def _choice_or_choice_list(value: Any, allowed: set[str], field_name: str) -> str | list[str]:
    if isinstance(value, list):
        return [_choice(item, allowed, field_name) for item in value]
    return _choice(value, allowed, field_name)


def _int(value: Any, field_name: str) -> int:
    if isinstance(value, bool):
        raise ValueError(f"{field_name} must be an integer.")
    try:
        return int(value)
    except (TypeError, ValueError) as exc:
        raise ValueError(f"{field_name} must be an integer.") from exc


def _positive_int(value: Any, field_name: str) -> int:
    normalized = _int(value, field_name)
    if normalized <= 0:
        raise ValueError(f"{field_name} must be positive.")
    return normalized


def _optional_positive_int(value: Any, field_name: str) -> int | None:
    if value is None or value == "":
        return None
    return _positive_int(value, field_name)


def _int_or_int_list(value: Any, field_name: str) -> int | list[int]:
    if isinstance(value, list):
        return [_int(item, field_name) for item in value]
    return _int(value, field_name)


def _float(value: Any, field_name: str) -> float:
    if isinstance(value, bool):
        raise ValueError(f"{field_name} must be a number.")
    try:
        return float(value)
    except (TypeError, ValueError) as exc:
        raise ValueError(f"{field_name} must be a number.") from exc


def _optional_float(value: Any, field_name: str) -> float | None:
    if value is None or value == "":
        return None
    return _float(value, field_name)


def _optional_positive_float(value: Any, field_name: str) -> float | None:
    normalized = _optional_float(value, field_name)
    if normalized is not None and normalized <= 0:
        raise ValueError(f"{field_name} must be positive.")
    return normalized
