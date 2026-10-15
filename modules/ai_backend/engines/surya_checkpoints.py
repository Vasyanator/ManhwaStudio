"""
File: modules/ai_backend/engines/surya_checkpoints.py

Purpose:
Presence check of the Surya checkpoint directories that Rust downloads, and the
one place that points Surya's process-global settings at them, for the
Surya-backed services (`ocr/surya.py`, `detection/surya.py`).

Why it exists:
Rust owns the Surya weights (pinned external catalog, `manifest.json` plus the
files it lists): the backend receives each absolute checkpoint directory per
request and loads it offline. Left alone, Surya resolves its built-in `s3://`
defaults (`surya/settings.py`) and downloads them inside the very
`from_pretrained` call that builds the model
(`surya/common/s3.py::S3DownloaderMixin.from_pretrained` -> `download_directory`).
A local directory instead goes straight to transformers' `from_pretrained`, so
proving the directory complete first and passing it as `checkpoint=` is what
keeps every Surya load free of network access.

Key functions:
- `checkpoint_ready()`: whether a directory holds every file of its manifest;
- `require_local_checkpoint()`: the same as a typed precondition with an
  explicit "not downloaded" error;
- `point_settings_at()`: assign the local directories to Surya's checkpoint
  settings, so no code path inside the library falls back to an `s3://` default.

Notes:
The Surya package is imported lazily, so a backend installation without it can
still import the services; every missing-package path raises with context
instead of guessing.
"""

from __future__ import annotations

import logging
from pathlib import Path

log = logging.getLogger(__name__)


def checkpoint_ready(local_dir: str) -> bool:
    """Whether `local_dir` holds every file listed in its `manifest.json`.

    `False` for an empty path and when Surya is not importable: the answer is
    "cannot prove the checkpoint is complete", never an optimistic yes.
    """
    normalized = str(local_dir or "").strip()
    if not normalized:
        return False
    try:
        from surya.common.s3 import check_manifest  # type: ignore
    except Exception as exc:
        log.debug("Cannot verify the Surya manifest in %s: %s", normalized, exc)
        return False
    return bool(check_manifest(normalized))


def require_local_checkpoint(local_dir: str, *, label: str) -> str:
    """Return `local_dir` when it is an absolute directory with a complete manifest.

    `label` names the model in error messages (for example
    `"Surya OCR recognition"`). Never downloads: the directory is Rust's, so an
    absent or incomplete one is reported as "not downloaded".

    # Raises
    `ValueError` for an empty or relative path, `FileNotFoundError` ("not
    downloaded") when the directory is missing or its `manifest.json` is
    missing, unreadable or lists a missing file, and `RuntimeError` when the
    Surya package is not importable (the manifest cannot be checked).
    """
    normalized = str(local_dir or "").strip()
    if not normalized:
        raise ValueError(f"{label} checkpoint directory is required.")
    if not Path(normalized).is_absolute():
        raise ValueError(f"{label} checkpoint directory must be absolute: {normalized}")
    if not Path(normalized).is_dir():
        raise FileNotFoundError(
            f"{label} model is not downloaded: directory does not exist: {normalized}"
        )
    try:
        from surya.common.s3 import check_manifest  # type: ignore
    except Exception as exc:
        raise RuntimeError(f"{label}: the Surya package is not available: {exc}") from exc
    if not check_manifest(normalized):
        raise FileNotFoundError(
            f"{label} model is not downloaded: {normalized} has no complete manifest.json"
        )
    return normalized


def point_settings_at(*, detection_dir: str | None = None, recognition_dir: str | None = None) -> None:
    """Assign local checkpoint directories to Surya's process-global settings.

    `detection_dir` sets `DETECTOR_MODEL_CHECKPOINT`; `recognition_dir` sets
    `FOUNDATION_MODEL_CHECKPOINT` and `RECOGNITION_MODEL_CHECKPOINT` (one
    checkpoint serves both in surya-ocr 0.17.1). `None` leaves a setting as it
    is. Callers pass the directory as `checkpoint=` too; this is the second
    line, so a library default read elsewhere (a loader or a tokenizer built
    without an explicit checkpoint) cannot resolve an `s3://` default and
    download it. Call it under the service lock, right before construction.

    # Raises
    `RuntimeError` when `surya.settings` is not importable.
    """
    try:
        from surya.settings import settings  # type: ignore
    except Exception as exc:
        raise RuntimeError(f"Surya settings are not available: {exc}") from exc
    if detection_dir is not None:
        settings.DETECTOR_MODEL_CHECKPOINT = detection_dir
    if recognition_dir is not None:
        settings.FOUNDATION_MODEL_CHECKPOINT = recognition_dir
        settings.RECOGNITION_MODEL_CHECKPOINT = recognition_dir
