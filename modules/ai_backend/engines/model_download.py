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
- stream a `requests` response body to disk with cumulative byte progress;
- OPTIONALLY keep the partial bytes of a failed transfer so the next run can
  resume it instead of restarting a multi-gigabyte file from zero.

Key functions:
- `download_to_path()` — the lock + staging + verify + atomic rename envelope.
- `stream_response_to_file()` — response body -> file, with `on_chunk` progress.
- `download_bearer_to_path()` — the two composed for a `Authorization: Bearer`
  endpoint, which is what every Hugging Face caller needs.
- `is_present()` / `staging_path()` / `resumable_staging_path()`
- `staged_bytes()` / `discard_staging()`

Notes:
The GENERIC transport is deliberately NOT owned here: the Google Drive caller
needs a `requests.Session` carrying Drive's confirm cookie, so it passes its own
`fetch(staging_path)` callable and an optional `verify(staging_path)` that raises
when the bytes are not a usable model file.

The Hugging Face half of that split IS owned here, as `download_bearer_to_path`:
both HF callers (`inpaint/flux_fill.py` and `inpaint/flux2_download.py`) need the
identical "bearer header + streaming GET + staged publish" composition, and a
second private copy of it is what this function exists to prevent. It takes the
token as an ARGUMENT and never reads the environment: one caller reads `HF_TOKEN`
from the process environment, the other receives the token as a request field,
and this layer must not decide which. The token is never logged and never
interpolated into an error message.

The download lock is intentionally NOT a service lock: a multi-GiB transfer must
never block `health()`, `unload()` or a `LoadedModelManager` eviction callback.
It is therefore held across network I/O and must never be nested inside a
service-local lock.

RESUME IS OPT-IN (`resumable=True`). By default a failed transfer's staging file
is unlinked, which is what `inpaint/flux_fill.py` has always relied on and what
its tests pin. Only `inpaint/flux2_download.py` asks for resumable staging, where
a dropped connection at 8 of 9.8 GB must not mean starting over. See
"Resumable staging" below.
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


def resumable_staging_path(dest: Path) -> Path:
    """Stable, pid-free staging file where a resumable transfer parks its bytes.

    A transfer always WRITES into the pid-scoped `staging_path`, so the
    cross-process safety of that name is untouched. This second name exists only
    between runs: a failed resumable transfer renames its partial bytes here, and
    the next run claims them back with another rename. `.part` never collides
    with `staging_path` because a pid is never empty.
    """
    return dest.with_name(f"{dest.name}.part")


def staged_bytes(dest: Path | str) -> int:
    """Bytes already parked for `dest` by an earlier resumable transfer.

    `0` when there is no parked file, which is also the answer when the path
    cannot be queried. Callers use it to size a free-space guard: demanding the
    full file again when 8 of its 9.8 GB are already on disk would refuse a
    download that will actually fit.
    """
    try:
        return int(os.path.getsize(resumable_staging_path(Path(dest))))
    except OSError:
        return 0


def discard_staging(dest: Path | str) -> None:
    """Throw away every partial byte staged for `dest`, both names.

    Used when the staged bytes are known to be WRONG rather than merely
    incomplete — a resumed file that failed its size gate, for instance, where
    resuming again from the same bytes could only fail the same way.
    """
    dest = Path(dest)
    _unlink_quietly(staging_path(dest))
    _unlink_quietly(resumable_staging_path(dest))


def download_to_path(
    dest: Path | str,
    fetch: Callable[[Path], None],
    *,
    verify: Callable[[Path], None] | None = None,
    resumable: bool = False,
) -> bool:
    """Fetch `dest` unless it is already on disk. Returns whether a transfer ran.

    `fetch(staging)` must write the complete payload into the staging path it is
    given; `verify(staging)`, when supplied, must raise if those bytes are not a
    usable model file. `dest` only ever appears complete: it is published with a
    single `os.replace` after `verify` passed, so a partial or rejected download
    is never visible under the final name.

    Concurrent calls for the same `dest` are serialized; the loser re-checks and
    returns `False` instead of refetching. Calls for different destinations run
    in parallel.

    `resumable` decides what happens to the partial bytes of a FAILED transfer,
    and nothing else:

    - `False` (the default, and `flux_fill.py`'s long-standing behaviour): the
      staging file is unlinked, so the next attempt starts from zero.
    - `True`: before `fetch` runs, an existing `resumable_staging_path` is
      ATOMICALLY RENAMED into this process's `staging_path`, and on failure it is
      renamed back. `fetch` therefore finds whatever bytes survived an earlier
      run at the staging path it is handed and may continue from there; it is
      `fetch`'s job to look, because only it knows whether the transport can
      resume at all. The rename is what keeps the pid-scoped name meaningful: a
      partial file has exactly one owner at a time, and a second process whose
      rename loses the race simply starts from zero.

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
        stable = resumable_staging_path(dest)
        if resumable:
            _claim_staged_bytes(staging, stable)
        published = False
        try:
            fetch(staging)
            if verify is not None:
                verify(staging)
            os.replace(staging, dest)
            published = True
        finally:
            if not published:
                if resumable:
                    _park_staged_bytes(staging, stable)
                else:
                    _unlink_quietly(staging)
    return True


def _claim_staged_bytes(staging: Path, stable: Path) -> None:
    """Take ownership of a parked partial file, if there is one.

    The rename is the claim: it is atomic, so of two processes racing for the
    same parked file exactly one gets it and the other sees `FileNotFoundError`
    and starts from zero. Nothing is shared and nothing is locked across
    processes.

    When there is nothing parked, any leftover pid-scoped staging file is
    REMOVED rather than resumed: it can only come from a process that crashed and
    happened to share our pid, so its bytes have no established relationship to
    this destination.
    """
    try:
        os.replace(stable, staging)
        return
    except FileNotFoundError:
        pass
    except OSError as exc:
        log.warning("model download: could not claim the staged file %s: %s", stable, exc)
    _unlink_quietly(staging)


def _park_staged_bytes(staging: Path, stable: Path) -> None:
    """Keep the partial bytes of a failed resumable transfer for the next run.

    Also atomic, and it OVERWRITES whatever was parked. Two processes failing on
    the same file at once can therefore leave the shorter of two partials, which
    costs some re-transfer on the next run and nothing else — the publish-time
    gate is what guarantees correctness, not the length of a parked file.
    """
    try:
        os.replace(staging, stable)
    except FileNotFoundError:
        return
    except OSError as exc:
        log.warning("model download: could not park the staged file %s: %s", staging, exc)
        _unlink_quietly(staging)


def stream_response_to_file(
    response: Any,
    dest: Path | str,
    on_chunk: ChunkCb,
    *,
    chunk_size: int = 1 << 20,
    mode: str = "wb",
    initial_done: int = 0,
) -> None:
    """Write a streaming `requests` response body to `dest`.

    `on_chunk(done, expected)` is called once per received chunk with the
    cumulative byte count and the total the server announced (`0` when it sent
    no `Content-Length`).

    `mode` and `initial_done` exist for resumed transfers and must agree: a
    `206 Partial Content` body is appended with `mode="ab"` and
    `initial_done=<bytes already on disk>`, so `done` counts the WHOLE file from
    its first chunk and neither an overall nor a per-file progress bar jumps
    backwards. `expected` is likewise the whole file (`initial_done` plus this
    body's `Content-Length`), because a range response announces only the length
    of the range. A caller that appends without setting `initial_done` would
    report a file restarting at zero; one that sets `initial_done` without
    `mode="ab"` would truncate the bytes it meant to keep.

    # Errors
    Raises `RuntimeError` when `dest` cannot be opened or written. Transport
    errors are left to propagate to the caller, which owns that diagnosis —
    `requests`' own exceptions derive from `OSError` and are indistinguishable
    from a disk failure here.
    """
    announced = _content_length(response)
    # `0` keeps its documented meaning of "the server did not say", so a resumed
    # transfer with no Content-Length reports unknown rather than a total that is
    # merely the offset.
    expected = initial_done + announced if announced else 0
    done = initial_done
    try:
        handle = open(dest, mode)
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


def download_bearer_to_path(
    url: str,
    dest: Path | str,
    on_chunk: ChunkCb,
    *,
    token: str | None,
    verify: Callable[[Path], None] | None = None,
    resumable: bool = False,
    timeout: float = 60.0,
    chunk_size: int = 1 << 20,
) -> bool:
    """Fetch `url` into `dest` over an `Authorization: Bearer` GET. Returns whether it ran.

    Composes `download_to_path` with `stream_response_to_file`, which is the
    exact shape both Hugging Face callers need: the per-destination lock, the
    process-private `.part` staging file and the atomic publish all apply, so two
    threads that start the same file at once transfer it once and the destination
    never appears half-written.

    `token` is supplied by the CALLER and is used only to build the request
    header; an empty or `None` token sends no `Authorization` header at all, which
    is correct for a public file. This function never reads the environment and
    never logs, returns or raises the token — a caller's own environment lookup
    stays the caller's business.

    `on_chunk(done, expected)` receives the cumulative bytes of THIS file and the
    announced `Content-Length` (`0` when the server sent none). It **may raise**
    to abort the transfer at a chunk boundary — that is the supported
    cancellation mechanism: the exception propagates to the caller and
    `download_to_path` removes the staging file on the way out, so nothing
    partial is ever published.

    `verify(staging)` is forwarded to `download_to_path` and runs on the staged
    bytes BEFORE the publish. **A caller that knows the expected length should
    pass one.** Neither this function nor `stream_response_to_file` compares the
    received bytes against `Content-Length`: a body that ends CLEANLY but short —
    a proxy or a transport closing without raising — otherwise reaches the
    publish as a success and is renamed to the final name, where a truncated file
    is indistinguishable from a complete one. That was reproduced (a body
    advertising 100 bytes, yielding 7, ending normally), so it is a real failure
    mode and not a hypothetical.

    `resumable` is forwarded to `download_to_path` AND turns on `Range` requests:
    when partial bytes were claimed, the GET asks for `bytes=<n>-` and every
    answer the server may legally give is handled — see `_bearer_attempt`. A
    server with no range support costs one restart, never a corrupt file.

    # Errors
    Propagates `requests` transport errors and `requests.HTTPError` for a non-2xx
    status (the caller owns the diagnosis, because only it knows whether a 401
    means "no token" or "wrong token"), `RuntimeError` when the staging file
    cannot be written, and whatever `on_chunk` or `verify` raised.
    """

    def fetch(staging: Path) -> None:
        resume_from = _existing_size(staging) if resumable else 0
        if resume_from > 0 and _bearer_attempt(
            url, staging, on_chunk, token=token, resume_from=resume_from,
            timeout=timeout, chunk_size=chunk_size,
        ):
            return
        # Either there was nothing to resume, or the server refused the range
        # (416) and the staged bytes are unusable. Fetch the whole file.
        _bearer_attempt(
            url, staging, on_chunk, token=token, resume_from=0,
            timeout=timeout, chunk_size=chunk_size,
        )

    return download_to_path(dest, fetch, verify=verify, resumable=resumable)


def _bearer_attempt(
    url: str,
    staging: Path,
    on_chunk: ChunkCb,
    *,
    token: str | None,
    resume_from: int,
    timeout: float,
    chunk_size: int,
) -> bool:
    """One bearer GET into `staging`. Returns `False` when it must be redone from zero.

    `resume_from > 0` sends `Range: bytes=<n>-`, and each legal answer is treated
    differently because getting this wrong corrupts a file rather than failing:

    - **206 Partial Content** — the normal resume: append, counting progress from
      `resume_from`.
    - **200 OK** — the server IGNORED the range and is sending the whole file.
      The staging file is TRUNCATED and rewritten from zero. Appending a full
      body to existing bytes would produce a silently double-length file that the
      transport reports as a success.
    - **416 Range Not Satisfiable** — the staged bytes are at or past the file's
      length, so they cannot be continued. Returns `False`; the caller discards
      them and refetches.
    - anything else — `raise_for_status()`, i.e. the existing error path.
    """
    import requests

    headers: dict[str, str] = {}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    if resume_from > 0:
        headers["Range"] = f"bytes={resume_from}-"

    with requests.get(
        url, stream=True, allow_redirects=True, headers=headers, timeout=timeout
    ) as response:
        status = int(getattr(response, "status_code", 200) or 200)
        if resume_from > 0 and status == 416:
            log.info(
                "model download: the server rejected the resume range for %s (416); "
                "refetching the whole file",
                staging.name,
            )
            return False
        response.raise_for_status()
        if resume_from > 0 and status != 206:
            log.info(
                "model download: the server ignored the resume range for %s (status %d); "
                "rewriting the file from the start",
                staging.name,
                status,
            )
            resume_from = 0
        stream_response_to_file(
            response,
            staging,
            on_chunk,
            chunk_size=chunk_size,
            mode="ab" if resume_from > 0 else "wb",
            initial_done=resume_from,
        )
    return True


def _existing_size(path: Path) -> int:
    """Size of `path`, or `0` when it does not exist or cannot be queried."""
    try:
        return int(os.path.getsize(path))
    except OSError:
        return 0


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
