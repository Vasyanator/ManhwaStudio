"""
File: modules/ai_backend/engines/model_download.py

Purpose:
Shared on-demand download primitive for the service domains that fetch their
own files: `inpaint/flux_fill.py` and `inpaint/hf_gated_download.py` (Hugging Face,
bearer token), `watermark/service.py` (Google Drive, confirm interstitial),
`watermark/code_fetch.py` (pinned upstream source files) and `reline/service.py`
(user-chosen catalog models).

Main responsibilities:
- serialize concurrent transfers targeting the same destination path;
- re-check the destination after taking that lock, so the loser of a race does
  not refetch gigabytes that are already on disk;
- stage every transfer into a process-private `<name>.<pid>.part` file and
  publish it with a single atomic `os.replace`;
- run a caller-supplied integrity gate on the staged bytes BEFORE publishing;
- OPTIONALLY keep the partial bytes of a failed transfer so the next run can
  resume it instead of restarting a multi-gigabyte file from zero;
- for every HTTP(S) GET download, survive an unstable network the way
  `PROJECT_RULES.md` "Network downloads" demands: transient failures are
  retried with capped exponential backoff and a `Range` resume, the retry budget
  resets after progress, cancellation interrupts the transfer and the backoff,
  and the bytes are checked against the expected size (and sha256 when pinned);
  an interstitial page (Google Drive's virus-scan confirm form) is followed
  INSIDE each attempt, so a retry repeats the confirm step too.

Key structures:
- `RetryPolicy` — retry budget and backoff bounds.
- `RetryEvent` — what the `on_retry` sink receives before each backoff wait.
- `DownloadError` and its subclasses `SizeMismatchError`, `HashMismatchError`,
  `RetriesExhaustedError` — the permanent failures diagnosed here.
- `DownloadCancelled` — the caller's cancel signal was observed.

Key functions:
- `download_to_path()` — the lock + staging + verify + atomic rename envelope.
- `download_http_to_path()` — the retrying, resuming, verifying transfer over a
  plain GET, with optional extra headers, an interstitial `follow` hook and a
  caller `verify` gate.
- `download_bearer_to_path()` — the same transfer with an
  `Authorization: Bearer` header, which is what every Hugging Face caller needs.
- `fetch_small_body()` — the same network rules for a small in-memory document
  (a catalog or listing): retry, backoff, cancel, size ceiling, no staging.
- `retry_progress_label()` — the one user-facing retry text of the progress frames.
- `hub_lfs_sha256()` — the sha256 a Hugging Face listing entry announces.
- `is_present()` / `staging_path()` / `resumable_staging_path()`
- `staged_bytes()` / `discard_staging()`

Notes:
The retrying HTTP transfer is owned here, once, as `download_http_to_path`:
every self-downloading backend domain needs the identical "streaming GET +
retry + resume + verify + staged publish" composition, and a private copy of it
per domain is what this module exists to prevent. Transport specifics stay with
the caller through seams: `headers` (a User-Agent), `follow` (Drive's confirm
form, parsed by `watermark/service.py`), and `get` (the Drive caller passes a
`requests.Session.get`, which carries the confirm cookie between hops).

`download_bearer_to_path` takes the token as an ARGUMENT and never reads the
environment: one caller reads `HF_TOKEN` from the process environment, the
other receives the token as a request field, and this layer must not decide
which. No header value is ever logged or interpolated into an error message.

The download lock is intentionally NOT a service lock: a multi-GiB transfer must
never block `health()`, `unload()` or a `SmallModelManager` eviction callback.
It is therefore held across network I/O and must never be nested inside a
service-local lock.

`download_to_path`'s resume is OPT-IN (`resumable=True`);
`download_http_to_path` (and therefore `download_bearer_to_path`) always resumes.
`requests` is imported lazily, so importing this module stays cheap.
"""

from __future__ import annotations

import hashlib
import logging
import os
import threading
import time
from dataclasses import dataclass
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
    present: Callable[[Path], bool] = is_present,
) -> bool:
    """Fetch `dest` unless it is already on disk. Returns whether a transfer ran.

    "Already on disk" is `present(dest)`, by default `is_present` (non-empty). A
    caller that knows the expected length passes a stricter predicate, so a file
    left at the wrong size is refetched (and atomically replaced) instead of
    being skipped forever.

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
    if present(dest):
        return False

    with _lock_for(os.path.abspath(dest)):
        # Another thread may have completed this very download while we waited
        # on the lock; a multi-GiB refetch is not free.
        if present(dest):
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


# =====================================================================
#  HTTP transfer: retry, resume, follow an interstitial, verify
# =====================================================================
#: Connect and read timeouts of every transfer GET, in seconds. Separate on
#: purpose: a dead host must fail fast, while a slow but alive CDN may pause
#: between chunks for longer than it takes to connect.
CONNECT_TIMEOUT_S = 15.0
READ_TIMEOUT_S = 60.0

#: HTTP statuses that are worth retrying; every other 4xx is permanent.
_TRANSIENT_STATUSES = frozenset({408, 429})

#: A transport `GET` with the signature of `requests.get`.
GetFn = Callable[..., Any]

#: Interstitial hook of `download_http_to_path`: `(response, hop) -> url | None`.
#: Called on every successful (non-error status) response before its body is
#: read; returning a URL discards that response and GETs the URL instead (same
#: headers, same `Range`), returning `None` streams the response. `hop` is `0`
#: for the request's own URL and grows by one per followed URL. It may raise
#: (a permanent refusal, e.g. a second interstitial) — the error propagates.
FollowFn = Callable[[Any, int], "str | None"]

#: Followed URLs allowed per attempt; a `follow` that keeps redirecting past it
#: is a loop, not a confirm step.
_MAX_FOLLOW_HOPS = 3


@dataclass(frozen=True)
class RetryPolicy:
    """Retry budget and backoff bounds of `download_http_to_path`.

    `max_retries` counts consecutive transient failures WITHOUT progress: an
    attempt that staged new bytes before failing resets the count, so a long
    transfer over a flaky link is not killed by failures spread over hours.
    The backoff is `base_delay_s * 2**(n-1)` capped at `max_delay_s`; a
    `Retry-After` header replaces it, capped at `max_retry_after_s`. The cancel
    signal is polled every `cancel_poll_s` of a backoff wait. The defaults
    mirror the Rust engine's standard policy.
    """

    max_retries: int = 5
    base_delay_s: float = 2.0
    max_delay_s: float = 30.0
    max_retry_after_s: float = 60.0
    cancel_poll_s: float = 0.1

    def delay_for(self, failures: int, retry_after_s: float | None) -> float:
        """Seconds to wait before retry number `failures` (1-based)."""
        if retry_after_s is not None:
            return max(0.0, min(retry_after_s, self.max_retry_after_s))
        exponent = max(0, failures - 1)
        return min(self.base_delay_s * (2.0 ** exponent), self.max_delay_s)


@dataclass(frozen=True)
class RetryEvent:
    """One upcoming retry, reported to `on_retry` BEFORE its backoff wait.

    `attempt` is the 1-based retry number within the current budget and
    `max_attempts` the budget, so a consumer can show "retry 2/5 in 4 s".
    `staged_bytes` is what is already on disk for the file and will be resumed.
    `reason` is a short technical English description for logs, never a
    user-facing text and never carrying the token.
    """

    attempt: int
    max_attempts: int
    delay_s: float
    staged_bytes: int
    reason: str


#: Retry sink of `download_http_to_path`.
RetryCb = Callable[[RetryEvent], None]


def retry_progress_label(file_label: str, event: RetryEvent) -> str:
    """User-facing progress label while a download waits to retry `file_label`.

    The one owner of this text for every caller: the IPC progress frames carry
    it in their existing `label` field, which the Rust side shows verbatim, so
    a retry needs no new protocol field.
    """
    return (
        f"Сеть недоступна, повтор через {event.delay_s:.0f} с "
        f"(попытка {event.attempt}/{event.max_attempts}): {file_label}"
    )


class DownloadError(RuntimeError):
    """A permanent download failure diagnosed by this module.

    A `RuntimeError` on purpose: the IPC handlers forward `RuntimeError`
    messages to the user unchanged, and every message here is user-facing
    text that names the file but never the token.
    """


class SizeMismatchError(DownloadError):
    """The bytes do not match the expected size (overflow or a changed remote file).

    The staged bytes were deleted: they are proven wrong, so resuming them
    could only fail the same way.
    """

    def __init__(self, message: str, *, expected: int, actual: int) -> None:
        super().__init__(message)
        self.expected = expected
        self.actual = actual


class HashMismatchError(DownloadError):
    """The complete file's sha256 differs from the pinned one; staged bytes deleted."""


class RetriesExhaustedError(DownloadError):
    """The retry budget ran out; the last transient failure is `__cause__`.

    The staged bytes are KEPT (parked), so the next run resumes them.
    """


class DownloadCancelled(Exception):
    """The caller's cancel signal was observed during a transfer or a backoff wait.

    Deliberately not a `RuntimeError`, so no handler mistakes it for a readable
    refusal. The staged bytes are parked for the next run.
    """


class _TransientStatus(Exception):
    """A retryable HTTP status (408 / 429 / 5xx) with its optional `Retry-After`."""

    def __init__(self, status: int, retry_after_s: float | None) -> None:
        super().__init__(f"HTTP {status}")
        self.status = status
        self.retry_after_s = retry_after_s


class _ShortBody(Exception):
    """A body that ended cleanly before the expected length: resumable, not wrong."""

    def __init__(self, received: int, expected: int) -> None:
        super().__init__(f"body ended at {received} of {expected} bytes")


class _UnusableAnswer(Exception):
    """A 2xx answer that cannot be (the rest of) the file: wrong length, an HTML
    page, a 200 to a range request without the full length.

    Retried like a network failure, and raised BEFORE the staging file is opened,
    so a captive-portal or proxy page never becomes a staged prefix that a later
    `Range` resume would complete into a right-sized but corrupt file.
    """

    def __init__(self, reason: str) -> None:
        super().__init__(reason)


class _RestartFromZero(Exception):
    """The staged bytes cannot be continued (416, a 206 at the wrong offset, or
    leftovers longer than the expected size).

    They were truncated; the transfer restarts at once from zero. Not counted
    against the retry budget, because nothing failed on the network, and it
    cannot loop: a restart sends no `Range`, so neither cause can recur.
    """


def _default_get(url: str, **kwargs: Any) -> Any:
    """`requests.get`, imported on first use so this module stays import-cheap."""
    import requests

    return requests.get(url, **kwargs)


def _default_sleep(seconds: float) -> None:
    """`time.sleep`; a module function so tests can replace the one seam."""
    time.sleep(seconds)


def _transient_errors() -> tuple[type[BaseException], ...]:
    """The transport exceptions that are retried: connect/DNS/reset, timeouts, broken bodies."""
    from requests import exceptions as rex

    return (
        _TransientStatus,
        _ShortBody,
        _UnusableAnswer,
        rex.ConnectionError,
        rex.Timeout,
        rex.ChunkedEncodingError,
    )


def hub_lfs_sha256(sibling: Any) -> str | None:
    """The lowercase-hex sha256 a Hugging Face listing entry announces, or `None`.

    `sibling` is a `huggingface_hub` `RepoSibling` from a `files_metadata=True`
    listing; its `lfs` (a `BlobLfsInfo`, a mapping in older hub versions) carries
    the content digest of an LFS file. Non-LFS files (small JSON configs) have
    none and are then checked by size alone. Anything that is not 64 hex digits
    is ignored rather than turned into a check that can never pass.
    """
    lfs = getattr(sibling, "lfs", None)
    if lfs is None:
        return None
    digest = lfs.get("sha256") if isinstance(lfs, dict) else getattr(lfs, "sha256", None)
    if not isinstance(digest, str):
        return None
    digest = digest.strip().lower()
    if len(digest) != 64 or any(c not in "0123456789abcdef" for c in digest):
        return None
    return digest


def download_bearer_to_path(
    url: str,
    dest: Path | str,
    *,
    token: str | None,
    expected_size: int,
    expected_sha256: str | None = None,
    should_cancel: Callable[[], bool] | None = None,
    on_progress: ChunkCb,
    on_retry: RetryCb | None = None,
    get: GetFn | None = None,
    sleep: Callable[[float], None] | None = None,
    retry: RetryPolicy = RetryPolicy(),
    chunk_size: int = 1 << 20,
) -> bool:
    """`download_http_to_path` over an `Authorization: Bearer` GET. Returns whether it ran.

    `token` builds the header only; empty or `None` sends no `Authorization`.
    It is never read from the environment, logged, returned or raised. Every
    other argument, the error set and the retry/resume/verify contract are
    `download_http_to_path`'s.
    """
    headers = {"Authorization": f"Bearer {token}"} if token else None
    return download_http_to_path(
        url, dest, expected_size=expected_size, expected_sha256=expected_sha256,
        headers=headers, should_cancel=should_cancel, on_progress=on_progress,
        on_retry=on_retry, get=get, sleep=sleep, retry=retry, chunk_size=chunk_size,
    )


def download_http_to_path(
    url: str,
    dest: Path | str,
    *,
    expected_size: int,
    expected_sha256: str | None = None,
    headers: dict[str, str] | None = None,
    follow: FollowFn | None = None,
    verify: Callable[[Path], None] | None = None,
    should_cancel: Callable[[], bool] | None = None,
    on_progress: ChunkCb,
    on_retry: RetryCb | None = None,
    get: GetFn | None = None,
    sleep: Callable[[float], None] | None = None,
    retry: RetryPolicy = RetryPolicy(),
    chunk_size: int = 1 << 20,
) -> bool:
    """Fetch `url` into `dest` over a streaming GET. Returns whether a transfer ran.

    Runs inside `download_to_path` with `resumable=True`: the per-destination
    lock, the process-private staging file and the atomic publish all apply, and
    bytes parked by an earlier run are claimed and continued.

    - `expected_size` is the WHOLE file's length in bytes. When positive it is
      the integrity gate: a destination at another size is not "present" and is
      refetched, a body ending early is resumed, a body delivering more than
      that (or a `206` announcing another total) deletes the staged bytes and
      raises `SizeMismatchError`, and leftovers of an earlier run that are
      already longer are dropped and refetched. `0` means "unknown, the server's
      `Content-Length` decides": only a body shorter than its own announced
      length is then detected, and a non-empty destination counts as present.
    - `expected_sha256` (lowercase hex), when given, is checked on the complete
      file before the publish; a mismatch deletes the staged bytes.
    - `headers` are sent with every GET of every attempt (a `Range` is added for
      a resume). Their values are never logged.
    - `follow` is the interstitial hook (`FollowFn`); it runs inside each
      attempt, so a retry repeats the confirm step against a fresh page.
    - `verify(staging)`, when given, runs after the size and hash gates and
      must raise when the bytes are not a usable file; the bytes are then proven
      wrong and are deleted rather than parked.
    - `on_progress(done, total)` gets the cumulative bytes of THIS file, counted
      from the resume offset, and `expected_size` (or the announced length, `0`
      when unknown). It may raise to abort; the exception propagates and the
      staged bytes are parked.
    - `should_cancel()` is polled at every chunk and every `retry.cancel_poll_s`
      of a backoff wait; when it answers true `DownloadCancelled` is raised. A
      read that stalls mid-body is not interrupted: the cancel then lands when
      it times out, at most `READ_TIMEOUT_S` later.
    - `on_retry(event)` is called before each backoff wait.
    - `get` / `sleep` are the transport and the clock seams (tests inject them;
      the Drive caller passes a `Session.get`); `None` means `requests.get` /
      `time.sleep`. Every GET uses the `(CONNECT_TIMEOUT_S, READ_TIMEOUT_S)`
      timeouts.

    Transient (retried, resuming the staged bytes): `requests` `ConnectionError`,
    `Timeout`, `ChunkedEncodingError`, a short body, HTTP 408 / 429 / 5xx (with
    `Retry-After` honoured). Everything else is permanent and not retried.

    # Errors
    `requests.HTTPError` for a permanent non-2xx status (the caller owns the
    diagnosis, because only it knows whether a 401 means "no token" or "wrong
    token"); `RetriesExhaustedError` after `retry.max_retries` failures without
    progress; `SizeMismatchError` / `HashMismatchError` for proven-wrong bytes;
    `DownloadError` when `follow` keeps redirecting past `_MAX_FOLLOW_HOPS`;
    `DownloadCancelled`; `RuntimeError` when the staging file cannot be written;
    whatever `follow`, `verify` or `on_progress` raised; `OSError` from the
    publish.
    """
    fetch_get = get if get is not None else _default_get
    fetch_sleep = sleep if sleep is not None else _default_sleep
    expected = max(0, int(expected_size))
    base_headers = dict(headers or {})

    def present(path: Path) -> bool:
        if expected <= 0:
            return is_present(path)
        try:
            return path.is_file() and path.stat().st_size == expected
        except OSError:
            return False

    name = Path(dest).name

    def fetch(staging: Path) -> None:
        transient = _transient_errors()
        failures = 0
        # Furthest length ever staged in this run. Progress is measured against
        # it, not against the previous attempt: a server that ignores `Range`
        # restarts every attempt from zero, and "dropped later than last time"
        # must not reset the budget forever.
        furthest = _existing_size(staging)
        while True:
            _raise_if_cancelled(should_cancel, name)
            try:
                _http_attempt(
                    url, staging, on_progress, name=name, headers=base_headers, follow=follow,
                    expected_size=expected, should_cancel=should_cancel, get=fetch_get,
                    chunk_size=chunk_size,
                )
                break
            except _RestartFromZero:
                continue
            except transient as exc:
                staged = _existing_size(staging)
                if staged > furthest:
                    # Progress was made: the budget is for failures in a row,
                    # not for the lifetime of a multi-gigabyte transfer.
                    failures = 0
                    furthest = staged
                failures += 1
                reason = _describe_transient(exc)
                if failures > retry.max_retries:
                    log.error(
                        "model download: giving up on %s after %d retries without progress "
                        "(%s); %d bytes stay staged for the next run",
                        name, retry.max_retries, reason, staged,
                    )
                    raise RetriesExhaustedError(
                        f"Не удалось скачать файл модели: {name}\n"
                        f"Сеть не отвечает после {retry.max_retries} повторных попыток "
                        f"({reason}). Уже скачанные данные сохранены, следующая попытка "
                        "продолжит с того же места."
                    ) from exc
                delay = retry.delay_for(failures, getattr(exc, "retry_after_s", None))
                log.warning(
                    "model download: transient failure on %s (%s); retry %d/%d in %.1f s, "
                    "resuming from %d bytes",
                    name, reason, failures, retry.max_retries, delay, staged,
                )
                if on_retry is not None:
                    on_retry(RetryEvent(failures, retry.max_retries, delay, staged, reason))
                _sleep_cancellable(delay, should_cancel, fetch_sleep, retry.cancel_poll_s, name)
        _verify_complete(staging, Path(dest), expected, expected_sha256, should_cancel)
        if verify is not None:
            try:
                verify(staging)
            except Exception:
                # The caller rejected complete bytes: they are proven wrong, and
                # parking them would only make the next run resume garbage.
                _discard_wrong(staging)
                raise

    return download_to_path(dest, fetch, resumable=True, present=present)


#: Ceiling of `fetch_small_body`: a listing or catalog, never a model file.
SMALL_BODY_MAX_BYTES = 16 << 20


def fetch_small_body(
    url: str,
    *,
    label: str,
    headers: dict[str, str] | None = None,
    max_bytes: int = SMALL_BODY_MAX_BYTES,
    should_cancel: Callable[[], bool] | None = None,
    on_retry: RetryCb | None = None,
    get: GetFn | None = None,
    sleep: Callable[[float], None] | None = None,
    retry: RetryPolicy = RetryPolicy(),
) -> bytes:
    """GET a small document (a catalog, a listing) into memory under the transfer's network rules.

    Same timeouts, transient classification, backoff and `Retry-After` handling as
    `download_http_to_path`, but nothing is staged on disk: each retry refetches the whole
    body, which is why it is only for small documents. `label` names the request in logs and
    messages. A body shorter than its `Content-Length` is transient; one exceeding `max_bytes`
    is permanent. The budget counts consecutive failures (there is no partial progress to
    reset it).

    # Errors
    `requests.HTTPError` for a permanent non-2xx status; `RetriesExhaustedError` after
    `retry.max_retries` consecutive transient failures; `DownloadError` for a body over
    `max_bytes`; `DownloadCancelled` when `should_cancel()` answers true (polled per chunk and
    during backoff waits).
    """
    fetch_get = get if get is not None else _default_get
    fetch_sleep = sleep if sleep is not None else _default_sleep
    transient = _transient_errors()
    failures = 0
    while True:
        _raise_if_cancelled(should_cancel, label)
        try:
            return _small_body_attempt(
                url, label, headers=dict(headers or {}), max_bytes=max_bytes,
                should_cancel=should_cancel, get=fetch_get,
            )
        except transient as exc:
            failures += 1
            reason = _describe_transient(exc)
            if failures > retry.max_retries:
                log.error(
                    "model download: giving up on %s after %d retries (%s)",
                    label, retry.max_retries, reason,
                )
                raise RetriesExhaustedError(
                    f"Не удалось получить {label}\n"
                    f"Сеть не отвечает после {retry.max_retries} повторных попыток ({reason})."
                ) from exc
            delay = retry.delay_for(failures, getattr(exc, "retry_after_s", None))
            log.warning(
                "model download: transient failure on %s (%s); retry %d/%d in %.1f s",
                label, reason, failures, retry.max_retries, delay,
            )
            if on_retry is not None:
                on_retry(RetryEvent(failures, retry.max_retries, delay, 0, reason))
            _sleep_cancellable(delay, should_cancel, fetch_sleep, retry.cancel_poll_s, label)


def _small_body_attempt(
    url: str,
    label: str,
    *,
    headers: dict[str, str],
    max_bytes: int,
    should_cancel: Callable[[], bool] | None,
    get: GetFn,
) -> bytes:
    """One GET of `fetch_small_body`; returns the complete body.

    # Errors
    `_TransientStatus` / `_ShortBody` (retried by the caller), `requests.HTTPError`,
    `DownloadError` past `max_bytes`, `DownloadCancelled`.
    """
    with get(
        url,
        stream=True,
        allow_redirects=True,
        headers=headers,
        timeout=(CONNECT_TIMEOUT_S, READ_TIMEOUT_S),
    ) as response:
        status = int(getattr(response, "status_code", 200) or 200)
        if status in _TRANSIENT_STATUSES or 500 <= status <= 599:
            raise _TransientStatus(status, _retry_after_s(response))
        response.raise_for_status()
        announced = _content_length(response)
        body = bytearray()
        for chunk in response.iter_content(chunk_size=1 << 16):
            if not chunk:
                continue
            body.extend(chunk)
            if len(body) > max_bytes:
                log.error("model download: %s exceeds %d bytes; refusing it", label, max_bytes)
                raise DownloadError(f"Ответ слишком большой: {label} (больше {max_bytes} Б).")
            _raise_if_cancelled(should_cancel, label)
    if announced and len(body) < announced:
        raise _ShortBody(len(body), announced)
    return bytes(body)


def _http_attempt(
    url: str,
    staging: Path,
    on_progress: ChunkCb,
    *,
    name: str,
    headers: dict[str, str],
    follow: FollowFn | None,
    expected_size: int,
    should_cancel: Callable[[], bool] | None,
    get: GetFn,
    chunk_size: int,
) -> None:
    """One attempt continuing `staging`; returns only when the body is complete.

    An attempt is one GET, plus one per URL `follow` hands back (each with the
    same headers and `Range`); the status rules below apply to every hop.

    Each legal answer to a `Range` request is treated differently, because
    getting it wrong corrupts a file rather than failing:

    - **206** — append, but only after `Content-Range` proved the body starts at
      the staged length (otherwise truncate and `_RestartFromZero`), that its
      total matches `expected_size` (otherwise `SizeMismatchError`) and that its
      `Content-Length` ends exactly there (otherwise `_UnusableAnswer`).
    - **200** to a range request — the server ignored it. Accepted only with a
      `Content-Length` equal to `expected_size` (when known); the body is
      written to a separate file that replaces the partial only once it is
      LONGER than it, so a failing restart never costs the bytes already staged.
      Appending a whole body would make a double-length file.
    - any **2xx** whose length cannot be the file (a `Content-Length` other
      than `expected_size`), or an HTML page (`text/html`, when no `follow` hook
      owns pages and the length does not prove the file) — `_UnusableAnswer`,
      raised before staging is touched.

    `name` (the destination's file name) labels logs and user-facing errors.
    - **416** — the staged bytes cannot be continued: truncate, `_RestartFromZero`
      (as for leftovers already longer than `expected_size`, before any request).
    - **408 / 429 / 5xx** — `_TransientStatus`; any other error status —
      `raise_for_status()`.

    # Errors
    `_ShortBody` when the body ended before the expected length, plus the
    errors listed on `download_http_to_path`.
    """
    have = _existing_size(staging)
    if expected_size > 0 and have > expected_size:
        # Leftover bytes of an earlier run, longer than the file now is (the
        # remote blob changed). Proven wrong, so dropped — but nothing failed in
        # THIS transfer, so it starts over instead of refusing.
        log.warning(
            "model download: %s has %d staged bytes, more than the expected %d; "
            "discarding them and refetching the whole file",
            name, have, expected_size,
        )
        _discard_wrong(staging)
        raise _RestartFromZero()
    if expected_size > 0 and have == expected_size:
        # An earlier run staged every byte and failed only at the publish.
        return

    request_headers = dict(headers)
    if have > 0:
        request_headers["Range"] = f"bytes={have}-"

    current = url
    hop = 0
    while True:
        with get(
            current,
            stream=True,
            allow_redirects=True,
            headers=request_headers,
            timeout=(CONNECT_TIMEOUT_S, READ_TIMEOUT_S),
        ) as response:
            status = int(getattr(response, "status_code", 200) or 200)
            if have > 0 and status == 416:
                log.info(
                    "model download: the server rejected the resume range for %s (416); "
                    "refetching the whole file",
                    name,
                )
                _discard_wrong(staging)
                raise _RestartFromZero()
            if status in _TRANSIENT_STATUSES or 500 <= status <= 599:
                raise _TransientStatus(status, _retry_after_s(response))
            response.raise_for_status()

            next_url = follow(response, hop) if follow is not None else None
            if next_url is not None:
                hop += 1
                if hop > _MAX_FOLLOW_HOPS:
                    log.error(
                        "model download: %s kept answering with an interstitial page "
                        "after %d followed URLs",
                        name, _MAX_FOLLOW_HOPS,
                    )
                    raise DownloadError(
                        f"Не удалось скачать файл модели: {name}\n"
                        "Сервер раз за разом возвращает промежуточную страницу вместо файла."
                    )
                current = next_url
                continue

            announced = _content_length(response)
            offset = 0
            if have > 0 and status == 206:
                start, total = _content_range(response)
                if start != have:
                    log.warning(
                        "model download: %s answered a resume from %d bytes with a range "
                        "starting at %s; refetching the whole file",
                        name, have, start,
                    )
                    _discard_wrong(staging)
                    raise _RestartFromZero()
                if expected_size > 0 and total is not None and total != expected_size:
                    _discard_wrong(staging)
                    raise _size_mismatch(name, expected_size, total)
                if expected_size > 0 and announced and have + announced != expected_size:
                    raise _UnusableAnswer(
                        f"206 of {announced} bytes from {have} does not end at {expected_size}"
                    )
                offset = have
            else:
                _check_whole_body(
                    response, status, announced, have=have, expected_size=expected_size,
                    pages_followed=follow is not None,
                )

            whole = expected_size or (offset + announced if announced else 0)
            if have > 0 and offset == 0:
                log.info(
                    "model download: the server ignored the resume range for %s (status %d); "
                    "refetching into a side file, the %d staged bytes stay until it is longer",
                    name, status, have,
                )
                done = _stream_restart(
                    response, staging, on_progress, name=name, have=have, whole=whole,
                    expected_size=expected_size, should_cancel=should_cancel, chunk_size=chunk_size,
                )
            else:
                done = _stream_checked(
                    response, staging, on_progress, name=name, offset=offset, whole=whole,
                    expected_size=expected_size, should_cancel=should_cancel, chunk_size=chunk_size,
                )
        break

    required = expected_size or (offset + announced if announced else 0)
    if done < required:
        raise _ShortBody(done, required)


def _check_whole_body(
    response: Any,
    status: int,
    announced: int,
    *,
    have: int,
    expected_size: int,
    pages_followed: bool,
) -> None:
    """Refuse a whole-file (non-206) 2xx answer that cannot be the file, before staging.

    `announced` is its `Content-Length` (`0` when absent). With a known size the
    length must match it, and a 200 to a range request (`have > 0`) must carry
    it at all. An HTML page is refused unless a `follow` hook owns pages
    (`pages_followed`) or the length proves the body is the file.

    # Errors
    `_UnusableAnswer` (transient) naming the reason.
    """
    if expected_size > 0 and announced and announced != expected_size:
        raise _UnusableAnswer(
            f"status {status} body of {announced} bytes, expected {expected_size}"
        )
    if have > 0 and expected_size > 0 and not announced:
        raise _UnusableAnswer(f"status {status} to a range request without a Content-Length")
    length_proves_file = expected_size > 0 and announced == expected_size
    if not pages_followed and not length_proves_file and _is_html(response):
        raise _UnusableAnswer(f"status {status} answered with an HTML page")


def _is_html(response: Any) -> bool:
    """Whether `response` announces an HTML document (a portal or error page, never a model)."""
    try:
        content_type = str(response.headers.get("Content-Type") or "")
    except AttributeError:
        return False
    return "text/html" in content_type.lower()


def _restart_path(staging: Path) -> Path:
    """Side file a 200-to-a-range-request body is written to while the partial is kept."""
    return staging.with_name(f"{staging.name}.restart")


def _stream_restart(
    response: Any,
    staging: Path,
    on_progress: ChunkCb,
    *,
    name: str,
    have: int,
    whole: int,
    expected_size: int,
    should_cancel: Callable[[], bool] | None,
    chunk_size: int,
) -> int:
    """Stream a from-zero body next to a `have`-byte partial; returns the length of `staging` after.

    The body goes to `_restart_path(staging)`. Whatever ends the stream —
    success, a transient failure, a cancel, a raising callback — the side file
    replaces `staging` only when it got LONGER than the partial; otherwise it is
    deleted and the partial stays. Bytes proven wrong (an overflow) are deleted
    by `_stream_checked` and never promoted.
    """
    side = _restart_path(staging)
    try:
        return _stream_checked(
            response, side, on_progress, name=name, offset=0, whole=whole,
            expected_size=expected_size, should_cancel=should_cancel, chunk_size=chunk_size,
        )
    finally:
        if _existing_size(side) > have:
            os.replace(side, staging)
        else:
            _unlink_quietly(side)


def _stream_checked(
    response: Any,
    staging: Path,
    on_progress: ChunkCb,
    *,
    name: str,
    offset: int,
    whole: int,
    expected_size: int,
    should_cancel: Callable[[], bool] | None,
    chunk_size: int,
) -> int:
    """Write the body to `staging` from `offset`; returns the file length reached.

    Polls cancellation per chunk and stops at the first byte past
    `expected_size`: such bytes are proven wrong, so they are deleted instead of
    being parked for a resume. `done` counts the whole file from `offset`, so a
    resumed transfer's progress never jumps backwards.
    """
    done = offset
    try:
        handle = open(staging, "ab" if offset > 0 else "wb")
    except OSError as exc:
        raise RuntimeError(
            f"Не удалось сохранить скачиваемый файл: {staging}\nОшибка: {exc}"
        ) from exc
    with handle:
        for chunk in response.iter_content(chunk_size=chunk_size):
            if not chunk:
                continue
            if expected_size > 0 and done + len(chunk) > expected_size:
                handle.close()
                _discard_wrong(staging)
                raise _size_mismatch(name, expected_size, done + len(chunk))
            try:
                handle.write(chunk)
            except OSError as exc:
                raise RuntimeError(
                    f"Не удалось сохранить скачиваемый файл: {staging}\nОшибка: {exc}"
                ) from exc
            done += len(chunk)
            on_progress(done, whole)
            _raise_if_cancelled(should_cancel, name)
    return done


def _verify_complete(
    staging: Path,
    dest: Path,
    expected_size: int,
    expected_sha256: str | None,
    should_cancel: Callable[[], bool] | None,
) -> None:
    """Final gate on the staged file: the exact size, then the pinned sha256.

    # Errors
    `SizeMismatchError` / `HashMismatchError` (staged bytes deleted), or
    `DownloadCancelled` while hashing (bytes parked).
    """
    actual = _existing_size(staging)
    if expected_size > 0 and actual != expected_size:
        _discard_wrong(staging)
        raise _size_mismatch(dest.name, expected_size, actual)
    if not expected_sha256:
        return
    digest = hashlib.sha256()
    try:
        with open(staging, "rb") as handle:
            while True:
                _raise_if_cancelled(should_cancel, dest.name)
                block = handle.read(1 << 20)
                if not block:
                    break
                digest.update(block)
    except OSError as exc:
        raise RuntimeError(
            f"Не удалось прочитать скачанный файл для проверки: {staging}\nОшибка: {exc}"
        ) from exc
    found = digest.hexdigest()
    if found != expected_sha256.strip().lower():
        log.error(
            "model download: sha256 mismatch for %s (expected %s, got %s); staged bytes deleted",
            dest.name, expected_sha256, found,
        )
        _discard_wrong(staging)
        raise HashMismatchError(
            f"Скачанный файл модели повреждён: {dest.name}\n"
            f"Контрольная сумма SHA-256 не совпала (ожидалось {expected_sha256}, "
            f"получено {found}). Файл удалён, повторите загрузку."
        )


def _sleep_cancellable(
    delay_s: float,
    should_cancel: Callable[[], bool] | None,
    sleep: Callable[[float], None],
    poll_s: float,
    name: str,
) -> None:
    """Wait `delay_s` in `poll_s` slices, raising `DownloadCancelled` as soon as asked.

    `name` labels the transfer in the log and the cancel message.
    """
    slice_s = poll_s if poll_s > 0 else delay_s
    waited = 0.0
    # The epsilon stops float drift (20 x 0.1 != 2.0) from adding a stray slice.
    while delay_s - waited > 1e-9:
        _raise_if_cancelled(should_cancel, name)
        step = min(slice_s, delay_s - waited)
        sleep(step)
        waited += step
    _raise_if_cancelled(should_cancel, name)


def _raise_if_cancelled(should_cancel: Callable[[], bool] | None, name: str) -> None:
    """Raise `DownloadCancelled` naming the transfer `name` when the cancel signal is up."""
    if should_cancel is not None and should_cancel():
        log.info("model download: %s cancelled by the caller", name)
        raise DownloadCancelled(f"Загрузка файла отменена: {name}")


def _size_mismatch(name: str, expected: int, actual: int) -> SizeMismatchError:
    """The `SizeMismatchError` for the model file `name`, logged with both sizes."""
    log.error(
        "model download: %s has the wrong size (expected %d bytes, got %d); staged bytes deleted",
        name, expected, actual,
    )
    return SizeMismatchError(
        f"Размер скачиваемого файла модели не совпал с ожидаемым: {name}\n"
        f"Ожидалось: {expected} Б, получено: {actual} Б. Скачанные данные удалены, "
        "повторите загрузку.",
        expected=expected,
        actual=actual,
    )


def _discard_wrong(staging: Path) -> None:
    """Delete staged bytes that are proven wrong, so the failure path cannot park them."""
    _unlink_quietly(staging)


def _describe_transient(exc: BaseException) -> str:
    """Short English cause of a transient failure for logs and `RetryEvent.reason`."""
    if isinstance(exc, (_TransientStatus, _ShortBody, _UnusableAnswer)):
        return str(exc)
    return type(exc).__name__


def _retry_after_s(response: Any) -> float | None:
    """`Retry-After` in seconds when given as a number; `None` otherwise (dates included)."""
    try:
        raw = response.headers.get("Retry-After")
    except AttributeError:
        return None
    if raw is None:
        return None
    try:
        return max(0.0, float(str(raw).strip()))
    except ValueError:
        return None


def _content_range(response: Any) -> tuple[int | None, int | None]:
    """`(start, total)` of a `Content-Range: bytes a-b/total` header; `None` per unknown part."""
    try:
        raw = str(response.headers.get("Content-Range") or "")
    except AttributeError:
        return None, None
    unit, _, spec = raw.strip().partition(" ")
    if unit.lower() != "bytes" or not spec:
        return None, None
    span, _, total_text = spec.partition("/")
    start_text = span.partition("-")[0]
    try:
        start: int | None = int(start_text)
    except ValueError:
        start = None
    try:
        total: int | None = int(total_text)
    except ValueError:
        total = None
    return start, total


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
