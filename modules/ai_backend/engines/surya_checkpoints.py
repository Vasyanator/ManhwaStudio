"""
File: modules/ai_backend/engines/surya_checkpoints.py

Purpose:
Presence check and eager download of the `s3://` checkpoints used by the
Surya-backed services (`ocr/surya.py`, `detection/surya.py`).

Why it exists:
Surya resolves an `s3://` checkpoint lazily, inside the very `from_pretrained`
call that also builds the model and moves it onto the device
(`surya/common/s3.py::S3DownloaderMixin.from_pretrained` -> `download_directory`,
3 attempts with a 5 s sleep between them). A service that wraps such a call in
`rocm_mmap_transfer.patched_module_to` would hold a process-global patch around
minutes of network I/O on a clean install, which that helper's contract
explicitly forbids. Fetching the checkpoint through
`ensure_checkpoint_downloaded()` first leaves only the weight transfer inside
the patch.

Key functions:
- `checkpoint_local_dir()`: directory Surya keeps (or would keep) a checkpoint in;
- `checkpoint_ready()`: whether that directory holds a complete checkpoint;
- `ensure_checkpoint_downloaded()`: download once and verify the manifest.

Notes:
The Surya package is imported lazily, so a backend installation without it can
still import the services; every missing-package path raises with context
instead of guessing.
"""

from __future__ import annotations

import logging

log = logging.getLogger(__name__)

# Prefix Surya uses for checkpoints it downloads from its own object storage.
S3_PREFIX = "s3://"


def checkpoint_local_dir(checkpoint: str) -> str:
    """Local directory Surya uses for `checkpoint`, or `""` when unknown.

    A non-`s3://` value is already a local path and is returned unchanged.
    Returns `""` only when the Surya package - which owns the cache layout - is
    not importable, i.e. "cannot tell", never a guessed path.
    """
    normalized = str(checkpoint or "").strip()
    if not normalized.startswith(S3_PREFIX):
        return normalized
    try:
        from surya.common.s3 import S3DownloaderMixin  # type: ignore
    except Exception as exc:
        log.debug("Cannot resolve the Surya cache path for %s: %s", normalized, exc)
        return ""
    return str(S3DownloaderMixin.get_local_path(normalized))


def checkpoint_ready(local_dir: str) -> bool:
    """Whether `local_dir` holds every file listed in its `manifest.json`.

    `False` for an empty path and when Surya is not importable: the answer is
    "cannot prove the checkpoint is complete", which callers must treat as
    "download it", never as an optimistic yes.
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


def ensure_checkpoint_downloaded(checkpoint: str, *, label: str) -> None:
    """Make sure `checkpoint` is complete on disk, downloading it when it is not.

    `label` names the model in error messages (for example
    `"Surya OCR foundation"`). On return the checkpoint can be loaded without
    any further network access, which is what lets a caller keep
    `rocm_mmap_transfer.patched_module_to` around the weight transfer alone.
    A checkpoint that is already complete costs one manifest check.

    # Raises
    `FileNotFoundError` if a non-`s3://` checkpoint does not hold a complete
    checkpoint on disk, and `RuntimeError` if the Surya download helpers are
    missing, the cache path cannot be resolved, or the download finishes with an
    incomplete manifest.
    """
    normalized = str(checkpoint or "").strip()
    local_dir = checkpoint_local_dir(normalized)
    if local_dir and checkpoint_ready(local_dir):
        return
    if not normalized.startswith(S3_PREFIX):
        raise FileNotFoundError(f"{label} checkpoint not found: {normalized}")

    try:
        from surya.common.s3 import download_directory  # type: ignore
    except Exception as exc:
        raise RuntimeError(
            f"{label} download helpers are not available: {exc}"
        ) from exc

    if not local_dir:
        raise RuntimeError(
            f"{label} checkpoint path is invalid for download: {normalized}"
        )

    remote_dir = normalized.removeprefix(S3_PREFIX)
    log.info("%s checkpoint download start remote=%s local_dir=%s", label, remote_dir, local_dir)
    download_directory(remote_dir, local_dir)
    if not checkpoint_ready(local_dir):
        raise RuntimeError(
            f"{label} checkpoint download finished but manifest is incomplete: {local_dir}"
        )
    log.info("%s checkpoint ready local_dir=%s", label, local_dir)
