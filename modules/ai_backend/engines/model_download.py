"""
File: modules/ai_backend/engines/model_download.py

Purpose:
Shared on-demand weight-download primitive for the service domains that fetch
their own model files: `inpaint/flux_fill.py` (Hugging Face, bearer token) and
`watermark/service.py` (Google Drive, confirm interstitial).

Main responsibilities:
- serialize concurrent transfers targeting the same destination path;
- re-check the destination after taking that lock, so the loser of a race does
  not refetch gigabytes that are already on disk;
- stage every transfer into a process-private `<name>.<pid>.part` file and
  publish it with a single atomic `os.replace`;
- run a caller-supplied integrity gate on the staged bytes BEFORE publishing;
- stream a `requests` response body to disk with cumulative byte progress.

Key functions:
- `download_to_path()` — the lock + staging + verify + atomic rename envelope.
- `stream_response_to_file()` — response body -> file, with `on_chunk` progress.
- `is_present()` / `staging_path()`

Notes:
The transport itself is deliberately NOT owned here: the two callers authenticate
and negotiate differently (HF bearer header vs. a `requests.Session` carrying
Drive's confirm cookie). A caller passes a `fetch(staging_path)` callable that
writes the bytes wherever it is told to, and an optional `verify(staging_path)`
that raises when those bytes are not a usable model file.

The download lock is intentionally NOT a service lock: a multi-GiB transfer must
never block `health()`, `unload()` or a `LoadedModelManager` eviction callback.
It is therefore held across network I/O and must never be nested inside a
service-local lock.
"""

from __future__ import annotations

import logging
import os
import threading
from pathlib import Path
from typing import Any, Callable

log = logging.getLogger(__name__)

#: Cumulative-bytes progress sink: `(done_bytes, expected_bytes)`; `expected` is
#: `0` when the server announced no `Content-Length`.
ChunkCb = Callable[[int, int], None]

#: Guards `_target_locks` only, never held across I/O.
_registry_lock = threading.Lock()

#: One lock per absolute destination path. Targets are not known up front (the
#: FLUX component plan is built from a live repo listing), so the locks are
#: created on demand. They are never removed: the set is bounded by the number of
#: distinct model files a process ever downloads, and dropping a lock while a
#: thread still waits on it would defeat the serialization.
_target_locks: dict[str, threading.Lock] = {}


def _lock_for(key: str) -> threading.Lock:
    """The download lock of `key`, creating it on first use."""
    with _registry_lock:
        lock = _target_locks.get(key)
        if lock is None:
            lock = threading.Lock()
            _target_locks[key] = lock
        return lock


def is_present(path: Path | str) -> bool:
    """Whether `path` is an existing, non-empty regular file."""
    try:
        return os.path.isfile(path) and os.path.getsize(path) > 0
    except OSError:
        return False


def staging_path(dest: Path) -> Path:
    """Process-private staging file next to `dest`.

    The pid is part of the name because the per-target lock only covers threads
    of THIS process: a second backend instance, a CLI run or a stale process
    fetching the same model must not write into the same staging file.
    """
    return dest.with_name(f"{dest.name}.{os.getpid()}.part")


def download_to_path(
    dest: Path | str,
    fetch: Callable[[Path], None],
    *,
    verify: Callable[[Path], None] | None = None,
) -> bool:
    """Fetch `dest` unless it is already on disk. Returns whether a transfer ran.

    `fetch(staging)` must write the complete payload into the staging path it is
    given; `verify(staging)`, when supplied, must raise if those bytes are not a
    usable model file. `dest` only ever appears complete: it is published with a
    single `os.replace` after `verify` passed, and the staging file is removed
    on any failure, so a partial or rejected download leaves nothing behind.

    Concurrent calls for the same `dest` are serialized; the loser re-checks and
    returns `False` instead of refetching. Calls for different destinations run
    in parallel.

    # Errors
    Propagates whatever `fetch` or `verify` raised, and `OSError` when the
    destination directory cannot be created or the rename fails.
    """
    dest = Path(dest)
    if is_present(dest):
        return False

    with _lock_for(os.path.abspath(dest)):
        # Another thread may have completed this very download while we waited
        # on the lock; a multi-GiB refetch is not free.
        if is_present(dest):
            return False

        dest.parent.mkdir(parents=True, exist_ok=True)
        staging = staging_path(dest)
        published = False
        try:
            fetch(staging)
            if verify is not None:
                verify(staging)
            os.replace(staging, dest)
            published = True
        finally:
            if not published:
                _unlink_quietly(staging)
    return True


def stream_response_to_file(
    response: Any,
    dest: Path | str,
    on_chunk: ChunkCb,
    *,
    chunk_size: int = 1 << 20,
) -> None:
    """Write a streaming `requests` response body to `dest`.

    `on_chunk(done, expected)` is called once per received chunk with the
    cumulative byte count and the announced `Content-Length` (`0` when the
    server sent none).

    # Errors
    Raises `RuntimeError` when `dest` cannot be opened or written. Transport
    errors are left to propagate to the caller, which owns that diagnosis —
    `requests`' own exceptions derive from `OSError` and are indistinguishable
    from a disk failure here.
    """
    expected = _content_length(response)
    done = 0
    try:
        handle = open(dest, "wb")
    except OSError as exc:
        raise RuntimeError(
            f"Не удалось сохранить скачиваемый файл: {dest}\nОшибка: {exc}"
        ) from exc
    with handle:
        for chunk in response.iter_content(chunk_size=chunk_size):
            if not chunk:
                continue
            try:
                handle.write(chunk)
            except OSError as exc:
                raise RuntimeError(
                    f"Не удалось сохранить скачиваемый файл: {dest}\nОшибка: {exc}"
                ) from exc
            done += len(chunk)
            on_chunk(done, expected)


def _content_length(response: Any) -> int:
    """Announced `Content-Length` of `response`, or `0` when absent/unparsable."""
    try:
        return int(response.headers.get("Content-Length"))
    except (AttributeError, TypeError, ValueError):
        return 0


def _unlink_quietly(path: Path) -> None:
    """Remove `path`, tolerating the case where it was never created."""
    try:
        path.unlink()
    except FileNotFoundError:
        return
    except OSError as exc:
        log.warning("model download: could not remove the staging file %s: %s", path, exc)
