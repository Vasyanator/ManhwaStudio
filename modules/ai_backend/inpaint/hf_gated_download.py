"""
File: modules/ai_backend/inpaint/hf_gated_download.py

Purpose:
The model-agnostic half of a backend-side Hugging Face model download: a
manifest of repository PREFIXES resolved against live listings into a plan,
the per-repository access check with the fixed `state` taxonomy, and the
resumable transfer with two-level throttled progress, a free-space guard and
real cancellation. A model package (`flux2_download.py`, `flux3_download.py`)
supplies only WHAT it needs — a `DownloadTarget` (label, repositories with
their gated flag, manifest rules, destination root) — and maps its own wire
fields onto it.

Main responsibilities:
- build the plan from PREFIXES resolved against the live repo listing, never
  from hard-coded shard names, so a re-sharded repo fails loudly instead of
  producing half a model;
- answer the per-repository access check (`STATE_*`) the UI draws its links
  from; the gate is probed with `HfApi.auth_check`, never inferred from a
  listing (the metadata of a `gated: auto` repo is public);
- send the token only to a GATED repository (`repo_token`), at all three
  network sites (access probe, listing, file transfer);
- publish a file only at the size (and, for LFS, the sha256) the listing
  announced: the shared primitive `engines.model_download` retries transient
  failures with backoff and `Range` resume, and every retry is a progress
  frame whose `label` says so, never a failure.

Key structures:
- `RepoSpec`, `DownloadTarget`, `ManifestEntry`, `ListedFile`, `PlannedFile`;
- `DownloadCanceled` — typed cancellation raised at a chunk boundary;
- `ProgressCb`, `ProgressThrottle`.

Key functions:
- `resolve_plan()` — pure: listings + manifest -> plan;
- `is_complete_on_disk()` / `missing_files()` / `plan_totals()`;
- `classify_repo_error()` / `probe_repo_access()` / `repo_listing()` /
  `fetch_listings()`;
- `check_access()` — the `.download.check` answer body (`plan` NULLABLE);
- `download()` — the transfer; `require_free_space()`.

Notes:
The Hugging Face token is a REQUEST FIELD. This module never reads it from the
environment, never persists it, never logs it and never interpolates it into an
error message; a message that must mention it says "токен". Messages from
`huggingface_hub` are scrubbed of the token value before they leave.
`probe_repo_access`, `repo_listing`, `fetch_listings` and `_fetch_one` receive
the token TO SEND (already filtered by `repo_token`); the two entry points
(`check_access`, `download`) do the filtering.
"""

from __future__ import annotations

import logging
import os
import shutil
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Iterable, Mapping, NamedTuple, Protocol

from ..engines.model_download import (
    DownloadCancelled,
    RetryEvent,
    download_bearer_to_path,
    hub_lfs_sha256,
    retry_progress_label,
    staged_bytes,
)

log = logging.getLogger(__name__)

# --- Access states (`dev-docs/flux2_model_download.md` §3, FIXED) ----------
STATE_OK = "ok"
STATE_NO_TOKEN = "no_token"
STATE_INVALID_TOKEN = "invalid_token"
STATE_NOT_ACCEPTED = "not_accepted"
STATE_NOT_FOUND = "not_found"
STATE_NETWORK_ERROR = "network_error"

#: Suffixes never downloaded, whatever prefix resolves them: llama.cpp quants
#: (`*.gguf`) the `transformers` loaders cannot read, and sample images.
EXCLUDED_SUFFIXES = (".gguf", ".jpg", ".jpeg")

#: Exact file names never downloaded. A licence file is NOT one of them — a
#: model that needs it names it as an explicit manifest entry.
EXCLUDED_NAMES = ("README.md", ".gitattributes")

#: Free space demanded on top of the missing bytes before the first byte is
#: fetched. The staging `.part` file of the file in flight lives next to the
#: destination, so the margin also covers it.
FREE_SPACE_MARGIN_BYTES = 1 << 30

#: Progress frames: at most this many per second AND at least
#: `PROGRESS_MIN_BYTES` apart (a per-chunk report at tens of GB would be an IPC
#: storm).
PROGRESS_MAX_PER_SECOND = 10.0
PROGRESS_MIN_BYTES = 4 << 20

#: The `phase` every frame of a download carries.
PROGRESS_PHASE = "download"


class ProgressCb(Protocol):
    """Two-level progress sink of the download.

    `step` / `total` are the OVERALL bytes of the transfer, which is what keeps
    every single-level consumer correct. The three keyword arguments describe
    the file in flight and are optional: a consumer that does not know them
    ignores them, and a preparation or terminal frame omits them.
    """

    def __call__(
        self,
        phase: str,
        step: int,
        total: int,
        label: str,
        *,
        file_step: int | None = None,
        file_total: int | None = None,
        file_label: str | None = None,
    ) -> None: ...


class DownloadCanceled(Exception):
    """Raised at a chunk boundary or a retry wait when the caller's cancel signal went up.

    The handler maps this to the dispatcher's `Interrupted`. Files already
    published stay — they are complete and were renamed atomically; the bytes of
    the file in flight are parked by the shared primitive's resumable failure
    path, so the next run continues them. The shared primitive's own
    `DownloadCancelled` (raised while it waits out a retry) is translated into
    this type, so a handler has one cancellation type to answer.
    """


@dataclass(frozen=True)
class RepoSpec:
    """One repository a download touches; `gated` = the hub demands a token AND accepted conditions."""

    repo: str
    gated: bool


@dataclass(frozen=True)
class ManifestEntry:
    """One rule mapping repository content to a destination under the model root.

    `prefix` is interpreted by shape, deliberately, so a manifest reads like a
    contract table:
    - `""` — the whole repository;
    - ending in `"/"` — a directory; every file beneath it is taken;
    - anything else — that exact file.

    `dest` is the destination path relative to the root, `"/"`-separated. For
    a directory or whole-repo entry it is the destination DIRECTORY and the
    path below the prefix is appended to it; for a file entry it is the
    destination file itself.
    """

    repo: str
    prefix: str
    dest: str

    @property
    def is_directory(self) -> bool:
        """Whether this entry takes every file below `prefix` rather than one file."""
        return self.prefix == "" or self.prefix.endswith("/")

    def matches(self, path: str) -> bool:
        """Whether the repository path `path` belongs to this entry."""
        if self.is_directory:
            return path.startswith(self.prefix)
        return path == self.prefix

    def destination(self, path: str) -> str:
        """Destination of `path`, relative to the root and `"/"`-separated.

        Assumes `matches(path)`; callers filter first.
        """
        if not self.is_directory:
            return self.dest
        relative = path[len(self.prefix) :]
        return f"{self.dest}/{relative}" if self.dest else relative


@dataclass(frozen=True)
class DownloadTarget:
    """Everything the generic check and transfer need to know about one model download.

    - `label` names the model in logs and user messages ("FLUX.2 klein");
    - `repos` are the repositories to check and fetch from, in report order;
    - `manifest` the rules in force (every rule's repo must be in `repos`);
    - `root` the directory that receives the files;
    - `no_token_message` the refusal of `download` for an empty token on a
      request that touches a gated repository (model-specific wording).
    """

    label: str
    repos: tuple[RepoSpec, ...]
    manifest: tuple[ManifestEntry, ...]
    root: Path
    no_token_message: str

    @property
    def token_required(self) -> bool:
        """Whether this request needs a Hugging Face token at all (any repository is gated)."""
        return any(spec.gated for spec in self.repos)


class ListedFile(NamedTuple):
    """One file of a repository listing: its size and, for an LFS file, its sha256.

    `size` is in bytes (`0` when the hub announced none); `sha256` is lowercase
    hex, `None` for a non-LFS file (small configs), which is then checked by
    size alone.
    """

    size: int
    sha256: str | None = None


@dataclass(frozen=True)
class PlannedFile:
    """One file of the resolved download plan.

    `label` is the destination path relative to the root and is what the
    progress frames carry; `dest` is the absolute path on disk. `size` is the
    size the repository listing announced, `0` when it announced none;
    `sha256` the LFS digest it announced, `None` when there is none. Both gate
    the publish of a downloaded file.
    """

    repo: str
    source: str
    dest: str
    label: str
    size: int
    sha256: str | None = None


def repo_token(spec: RepoSpec, token: str) -> str:
    """The token to send to `spec.repo`: `token` for a gated repository, `""` otherwise.

    A public repository is contacted ANONYMOUSLY even when the request carries
    a token: an expired token forwarded to a public repository makes the hub
    answer 401, which would be reported as `invalid_token` for a repository
    that needs no token at all. The token value is never logged.
    """
    return token if spec.gated else ""


def log_anonymous_repos(target: DownloadTarget, token: str) -> None:
    """Record that a supplied token is deliberately withheld from public repositories.

    Silent when no token was supplied or when every repository is gated. Logs
    repository ids only — never the token.
    """
    if not token:
        return
    public = [spec.repo for spec in target.repos if not spec.gated]
    if not public:
        return
    log.info(
        "%s: репозитории %s публичные — запросы к ним выполняются без токена.",
        target.label,
        ", ".join(public),
    )


def is_excluded(path: str) -> bool:
    """Whether the repository path `path` is one of the files never downloaded.

    Applied to every entry's resolved files, including whole-repo ones, so the
    excluded names and suffixes are filtered no matter which rule pulled them in.
    """
    name = path.rsplit("/", 1)[-1]
    if name in EXCLUDED_NAMES:
        return True
    return any(name.lower().endswith(suffix) for suffix in EXCLUDED_SUFFIXES)


def resolve_plan(
    listings: Mapping[str, Mapping[str, ListedFile | int]],
    manifest: Iterable[ManifestEntry],
    root: Path | str,
) -> list[PlannedFile]:
    """Resolve the manifest prefixes against live repository listings.

    `listings` maps a repository id to `{path inside the repo: ListedFile}`,
    which is exactly what `repo_listing()` returns (a bare `int` size is also
    accepted and means "no digest"). Paths `is_excluded` names are skipped. The
    result is ordered by
    destination so a run is reproducible and a resumed run continues in the same
    order.

    # Errors
    Raises `RuntimeError` when a listing for a needed repository is missing, or
    when a prefix resolves to no file at all — the latter names the prefix and
    the repository, because it means the repository layout changed under us.
    """
    base = Path(root)
    planned: dict[str, PlannedFile] = {}

    for entry in manifest:
        listing = listings.get(entry.repo)
        if listing is None:
            raise RuntimeError(
                f"Нет списка файлов репозитория {entry.repo}: план загрузки построить нельзя."
            )
        matched = 0
        for path in sorted(listing):
            if not entry.matches(path) or is_excluded(path):
                continue
            matched += 1
            label = entry.destination(path)
            listed = listing[path]
            size, sha256 = (listed.size, listed.sha256) if isinstance(listed, ListedFile) else (listed, None)
            planned[label] = PlannedFile(
                repo=entry.repo,
                source=path,
                dest=str(base.joinpath(*label.split("/"))),
                label=label,
                size=max(0, int(size or 0)),
                sha256=sha256,
            )
        if matched == 0:
            raise RuntimeError(
                f"В репозитории {entry.repo} не найдено ни одного файла по пути "
                f"«{entry.prefix or '/'}». Состав репозитория изменился — обновите "
                f"ManhwaStudio или сообщите об ошибке; частичная модель не скачивается."
            )

    return [planned[label] for label in sorted(planned)]


def is_complete_on_disk(item: PlannedFile) -> bool:
    """Whether `item` is already on disk AT THE SIZE THE LISTING ANNOUNCED.

    Presence means the RIGHT size, not merely non-empty: a body that ended
    cleanly but short would otherwise be published under the final name and
    skipped by every later run. Comparing the length also REPAIRS a file an
    earlier run left behind at the wrong size.

    When the listing announced no size (`size == 0`) the non-empty rule stands,
    and the reason is logged: a missing size must not silently disable the
    check for a weights shard, but nor may it force an endless re-download.
    """
    try:
        if not os.path.isfile(item.dest):
            return False
        actual = int(os.path.getsize(item.dest))
    except OSError:
        return False
    if item.size > 0:
        if actual == item.size:
            return True
        log.warning(
            "%s: %s на диске имеет размер %d Б вместо объявленных %d Б — файл будет скачан заново.",
            item.repo,
            item.label,
            actual,
            item.size,
        )
        return False
    log.warning(
        "%s: репозиторий не сообщил размер файла %s — проверка длины невозможна, "
        "файл считается готовым по признаку непустоты.",
        item.repo,
        item.label,
    )
    return actual > 0


def missing_files(plan: Iterable[PlannedFile]) -> list[PlannedFile]:
    """The planned files that are absent, empty, or the wrong length on disk.

    Files already present at the announced size are skipped, so a re-run resumes
    at file granularity. See `is_complete_on_disk` for why the length is compared.
    """
    return [item for item in plan if not is_complete_on_disk(item)]


def plan_totals(plan: Iterable[PlannedFile]) -> dict[str, int]:
    """The `plan` object of the `.check` answer: total, missing bytes and count."""
    items = list(plan)
    missing = missing_files(items)
    return {
        "total_bytes": sum(item.size for item in items),
        "missing_bytes": sum(item.size for item in missing),
        "missing_files": len(missing),
    }


# =====================================================================
#  Hugging Face access
# =====================================================================
def scrub(message: str, token: str | None) -> str:
    """Remove the token value from `message`, whatever produced it.

    Belt and braces for "the token is never logged": nothing here interpolates
    the token, but a message that came back from `huggingface_hub` is not ours.
    """
    if token:
        message = message.replace(token, "<токен>")
    return message


def classify_repo_error(exc: BaseException) -> str:
    """Map a `huggingface_hub` failure to one of the fixed `state` values.

    The HTTP status is consulted BEFORE the exception type, and that order is the
    contract: on a gated repository a wrong token raises `GatedRepoError` with
    status 401, and the user must be told the token is wrong rather than that
    they failed to accept the conditions. `GatedRepoError` also SUBCLASSES
    `RepositoryNotFoundError`, so type order alone cannot separate 403 from 404.

    Measured against huggingface_hub 0.35.1: valid+accepted -> no exception;
    absent or invalid token -> `GatedRepoError` 401; valid token without accepted
    conditions -> `GatedRepoError` 403 (`X-Error-Code: GatedRepo`); unknown repo
    with a valid token -> `RepositoryNotFoundError` 404.
    """
    from huggingface_hub.errors import GatedRepoError

    status = getattr(getattr(exc, "response", None), "status_code", None)
    if status == 401:
        return STATE_INVALID_TOKEN
    if status == 403:
        return STATE_NOT_ACCEPTED
    if isinstance(exc, GatedRepoError):
        return STATE_NOT_ACCEPTED
    if status == 404:
        return STATE_NOT_FOUND
    return STATE_NETWORK_ERROR


def probe_repo_access(repo: str, token: str) -> tuple[str, str]:
    """Whether `token` (the token TO SEND, `repo_token`) may download `repo`. Returns `(state, message)`.

    `message` is empty for `ok` and carries the technical detail otherwise,
    scrubbed of the token. A listing is NOT a proof of access (`model_info()`
    answers for `gated: auto` repositories with no token); `auth_check` is the
    only call that answers this question. An EMPTY token is passed to `HfApi`
    as `None` rather than `""`: `huggingface_hub` treats a falsy token as
    "anonymous" only when it is not an empty string on every code path.
    """
    from huggingface_hub import HfApi

    try:
        HfApi(token=token or None).auth_check(repo)
    except Exception as exc:  # noqa: BLE001 - every failure is a reportable state
        return classify_repo_error(exc), scrub(str(exc), token)
    return STATE_OK, ""


def repo_listing(repo: str, token: str) -> dict[str, ListedFile]:
    """`{path inside the repo: ListedFile(size, sha256)}` for every file of `repo`.

    One metadata request, no file bytes; `token` is the token TO SEND.

    # Errors
    Propagates `huggingface_hub` errors; `check_access` reports them as
    `plan_error` and `download` turns them into a readable refusal.
    """
    from huggingface_hub import HfApi

    info = HfApi(token=token or None).model_info(repo, files_metadata=True)
    listing: dict[str, ListedFile] = {}
    for sibling in info.siblings or []:
        name = getattr(sibling, "rfilename", None)
        if not name:
            continue
        listing[str(name)] = ListedFile(
            max(0, int(getattr(sibling, "size", 0) or 0)), hub_lfs_sha256(sibling)
        )
    return listing


def fetch_listings(repos: Iterable[str], token: str) -> dict[str, dict[str, ListedFile]]:
    """Listings of every repository in `repos` (all sent the same `token`), keyed by repository id."""
    return {repo: repo_listing(repo, token) for repo in repos}


def _fetch_target_listings(target: DownloadTarget, token: str) -> dict[str, dict[str, ListedFile]]:
    """Listings of every repository of `target`, each fetched with its own `repo_token`."""
    listings: dict[str, dict[str, ListedFile]] = {}
    for spec in target.repos:
        listings.update(fetch_listings([spec.repo], repo_token(spec, token)))
    return listings


def check_access(
    hf_token: str,
    target: DownloadTarget,
    *,
    echo: Mapping[str, Any],
) -> dict[str, Any]:
    """The `.download.check` answer for `target`: `{**echo, repos, plan, plan_error}`.

    `echo` carries the request's selection fields (`{"variant": "9b"}`,
    `{"format": "fp8r"}`) so a client can tell a fresh answer from one
    computed for what it was showing a moment ago.

    An empty token short-circuits to `no_token` for every repository and makes
    NO network call at all — but ONLY when a repository is gated
    (`DownloadTarget.token_required`). A supplied token is still not sent
    everywhere: `repo_token` decides per repository.

    **`plan` is NULLABLE and a zeroed plan is never synthesised.** Access and
    listing are two different network operations: a token can pass `auth_check`
    and the listing can still fail. Zeros beside `ok` states read as
    "everything is already downloaded", so a plan that could not be computed is
    `None` and `plan_error` carries the reason (scrubbed of the token).
    `plan_error` is empty whenever `plan` is present, and also when a
    repository is inaccessible: its `state` already says what is wrong.

    # Errors
    Does not raise for an inaccessible repository or a failed listing — that is
    what the `state` taxonomy and `plan_error` are for.
    """
    token = (hf_token or "").strip()
    repos = tuple(spec.repo for spec in target.repos)

    if not token and target.token_required:
        return {
            **echo,
            "repos": {repo: {"state": STATE_NO_TOKEN, "message": ""} for repo in repos},
            "plan": None,
            "plan_error": "",
        }

    log_anonymous_repos(target, token)
    states: dict[str, dict[str, str]] = {}
    for spec in target.repos:
        state, message = probe_repo_access(spec.repo, repo_token(spec, token))
        if state != STATE_OK:
            log.info("%s: доступ к %s — %s", target.label, spec.repo, state)
        states[spec.repo] = {"state": state, "message": scrub(message, token)}

    plan_object: dict[str, int] | None = None
    plan_error = ""
    if all(entry["state"] == STATE_OK for entry in states.values()):
        try:
            listings = _fetch_target_listings(target, token)
            plan_object = plan_totals(resolve_plan(listings, target.manifest, target.root))
        except Exception as exc:  # noqa: BLE001 - reported as plan_error, not as a state
            # Deliberately NOT mapped to a repo state: access genuinely succeeded,
            # and calling this `network_error` would send the user to the wrong
            # link (accept the conditions / renew the token) for a listing that
            # merely could not be fetched.
            plan_error = scrub(str(exc), token)
            log.warning(
                "%s: доступ есть, но список файлов получить не удалось: %s",
                target.label,
                plan_error,
            )

    return {
        **echo,
        "repos": states,
        "plan": plan_object,
        "plan_error": plan_error,
    }


# =====================================================================
#  Free space
# =====================================================================
def _existing_ancestor(path: Path) -> Path:
    """Nearest existing directory at or above `path` (`shutil.disk_usage` needs one)."""
    candidate = path
    while True:
        if candidate.exists():
            return candidate
        parent = candidate.parent
        if parent == candidate:
            return candidate
        candidate = parent


def free_bytes(path: Path) -> int:
    """Free bytes on the filesystem holding `path` (or its nearest existing parent).

    Returns `-1` when the filesystem cannot be queried, which callers must read
    as "cannot tell" and never as "no space".
    """
    try:
        return int(shutil.disk_usage(_existing_ancestor(path)).free)
    except OSError as exc:
        log.warning("не удалось узнать свободное место в %s: %s", path, exc)
        return -1


def require_free_space(root: Path, required: int, *, label: str) -> None:
    """Refuse the download when `root`'s filesystem cannot hold `required` bytes.

    `required` is the missing-bytes total; `FREE_SPACE_MARGIN_BYTES` is added on
    top because the staging `.part` file of the file in flight lives next to its
    destination. Checked BEFORE the first byte. `label` names the model.

    # Errors
    Raises `RuntimeError` naming both the required and the available number of
    bytes. A filesystem that cannot be queried is NOT treated as full — the
    transfer proceeds and fails later with a disk error if it really was.
    """
    if required <= 0:
        return
    available = free_bytes(root)
    if available < 0:
        return
    needed = required + FREE_SPACE_MARGIN_BYTES
    if available >= needed:
        return
    raise RuntimeError(
        f"Недостаточно места на диске для загрузки модели {label}.\n"
        f"Требуется: {needed} Б ({needed / 1e9:.2f} ГБ, включая запас "
        f"{FREE_SPACE_MARGIN_BYTES / 1e9:.2f} ГБ).\n"
        f"Доступно: {available} Б ({available / 1e9:.2f} ГБ).\n"
        f"Каталог: {root}"
    )


# =====================================================================
#  Progress throttling
# =====================================================================
class ProgressThrottle:
    """Rate limiter for the progress frames of one download.

    A frame is emitted only when BOTH gates opened since the previous one: at
    least `min_interval` seconds AND at least `min_bytes` of progress.
    `force=True` bypasses both gates and is used for the terminal frame, so the
    last thing a consumer sees always reports the completed byte count.
    """

    def __init__(
        self,
        *,
        min_interval: float = 1.0 / PROGRESS_MAX_PER_SECOND,
        min_bytes: int = PROGRESS_MIN_BYTES,
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        self._min_interval = float(min_interval)
        self._min_bytes = int(min_bytes)
        self._clock = clock
        self._last_time = self._clock()
        self._last_bytes = 0

    def should_emit(self, done_bytes: int, *, force: bool = False) -> bool:
        """Whether a frame reporting `done_bytes` overall bytes may be sent now."""
        now = self._clock()
        if not force:
            if now - self._last_time < self._min_interval:
                return False
            if done_bytes - self._last_bytes < self._min_bytes:
                return False
        self._last_time = now
        self._last_bytes = done_bytes
        return True


# =====================================================================
#  The download itself
# =====================================================================
def download(
    hf_token: str,
    target: DownloadTarget,
    *,
    progress_callback: ProgressCb | None = None,
    should_cancel: Callable[[], bool] | None = None,
) -> dict[str, int]:
    """Fetch every missing file of `target`'s plan. Returns `{downloaded_bytes, skipped_files}`.

    Runs OUTSIDE every service lock — the download lock of
    `engines.model_download` is per destination path, because a multi-gigabyte
    transfer must never block `health()`, `unload()` or an eviction callback.

    `should_cancel()` is polled before every file, at every chunk boundary and
    during every retry wait. Files already published stay; the bytes of the one
    in flight are parked for the next run.

    # Errors
    Raises `ValueError(target.no_token_message)` for an empty token on a request
    that touches a gated repository (before any network call);
    `DownloadCanceled` when the caller canceled; and `RuntimeError` for an
    inaccessible repository, a manifest prefix that resolved to nothing,
    insufficient free space, or a transport failure (permanent, or transient
    with the retry budget exhausted — the `engines.model_download.DownloadError`
    family).
    """
    token = (hf_token or "").strip()
    if not token and target.token_required:
        raise ValueError(target.no_token_message)

    log_anonymous_repos(target, token)
    _check_canceled(should_cancel, target.label)
    try:
        listings = _fetch_target_listings(target, token)
    except Exception as exc:  # noqa: BLE001 - reported as a readable refusal
        raise RuntimeError(_listing_failure_message(exc, token, target.label)) from exc

    plan = resolve_plan(listings, target.manifest, target.root)
    pending = missing_files(plan)
    skipped = len(plan) - len(pending)
    sent_tokens = {spec.repo: repo_token(spec, token) for spec in target.repos}

    total = sum(item.size for item in pending)
    # Bytes an earlier run already parked next to their destinations are on the
    # disk already; demanding them a second time would refuse a download that
    # will actually fit.
    require_free_space(target.root, total - sum(staged_bytes(item.dest) for item in pending), label=target.label)

    throttle = ProgressThrottle()
    overall_done = 0
    downloaded = 0

    for item in pending:
        _check_canceled(should_cancel, target.label)
        on_chunk = _chunk_reporter(
            item,
            offset=overall_done,
            total=total,
            throttle=throttle,
            progress_callback=progress_callback,
            should_cancel=should_cancel,
            label=target.label,
        )
        on_retry = _retry_reporter(
            item,
            offset=overall_done,
            total=total,
            throttle=throttle,
            progress_callback=progress_callback,
        )
        ran = _fetch_one(
            item,
            token=sent_tokens[item.repo],
            on_chunk=on_chunk,
            on_retry=on_retry,
            should_cancel=should_cancel,
            label=target.label,
        )
        if ran:
            downloaded += _file_size_on_disk(item)
        else:
            # Another thread published it while we waited on the per-path lock.
            skipped += 1
        overall_done += item.size

    if progress_callback is not None:
        # The terminal frame always reports the completed byte count, whatever
        # the throttle allowed last. It carries no file level: nothing is in
        # flight any more, and the three fields are optional for exactly this.
        throttle.should_emit(total, force=True)
        progress_callback(PROGRESS_PHASE, total, total, "")

    return {"downloaded_bytes": downloaded, "skipped_files": skipped}


def _chunk_reporter(
    item: PlannedFile,
    *,
    offset: int,
    total: int,
    throttle: ProgressThrottle,
    progress_callback: ProgressCb | None,
    should_cancel: Callable[[], bool] | None,
    label: str,
) -> Callable[[int, int], None]:
    """Per-file chunk sink: cancellation check + throttled two-level progress.

    `offset` is the overall byte count already transferred before this file, so
    the frame's `step` stays an OVERALL figure while `file_step` describes the
    file in flight. The returned callable RAISES `DownloadCanceled` when the
    caller canceled; the exception propagates out of the shared streaming loop
    and the staged bytes are PARKED for a resume, not deleted.
    """

    def on_chunk(done: int, expected: int) -> None:
        _check_canceled(should_cancel, label)
        current = offset + done
        if progress_callback is not None and throttle.should_emit(current):
            progress_callback(
                PROGRESS_PHASE,
                current,
                total,
                item.label,
                file_step=done,
                file_total=item.size or expected,
                file_label=item.label.rsplit("/", 1)[-1],
            )

    return on_chunk


def _retry_reporter(
    item: PlannedFile,
    *,
    offset: int,
    total: int,
    throttle: ProgressThrottle,
    progress_callback: ProgressCb | None,
) -> Callable[[RetryEvent], None]:
    """Retry sink: one FORCED progress frame whose `label` announces the retry.

    The frame keeps the ordinary shape — `step` / `file_step` are the bytes
    already staged, which the retry resumes — so no consumer has to learn a new
    field. Forced, because a retry wait is exactly when the throttle would
    otherwise leave the bar silent for up to a minute.
    """

    def on_retry(event: RetryEvent) -> None:
        if progress_callback is None:
            return
        current = offset + event.staged_bytes
        throttle.should_emit(current, force=True)
        file_name = item.label.rsplit("/", 1)[-1]
        progress_callback(
            PROGRESS_PHASE,
            current,
            total,
            retry_progress_label(item.label, event),
            file_step=event.staged_bytes,
            file_total=item.size,
            file_label=file_name,
        )

    return on_retry


def _fetch_one(
    item: PlannedFile,
    *,
    token: str,
    on_chunk: Callable[[int, int], None],
    on_retry: Callable[[RetryEvent], None],
    should_cancel: Callable[[], bool] | None,
    label: str,
) -> bool:
    """Fetch one planned file, resuming what an earlier run left. Returns whether it ran.

    `token` is the token TO SEND for `item.repo` (`repo_token`; empty for a
    public repository, so no `Authorization` header goes out).

    Resume, retry and the size gate all live in the shared primitive: a dropped
    connection or a body that ended cleanly but short is resumed with `Range`,
    and only bytes PROVEN wrong — more than the announced size, a `206`
    announcing a different total, or a complete file whose sha256 differs from
    the listing's LFS digest — are discarded.

    # Errors
    `DownloadCanceled` when the caller canceled during a retry wait (a cancel at
    a chunk boundary already raises it from `on_chunk`); everything else
    propagates from the shared primitive.
    """
    url = _file_url(item)
    try:
        return download_bearer_to_path(
            url,
            item.dest,
            token=token,
            expected_size=item.size,
            expected_sha256=item.sha256,
            should_cancel=should_cancel,
            on_progress=on_chunk,
            on_retry=on_retry,
        )
    except DownloadCancelled as exc:
        raise DownloadCanceled(f"Загрузка модели {label} отменена.") from exc


def _file_url(item: PlannedFile) -> str:
    """Resolve the download URL of one planned file."""
    from huggingface_hub import hf_hub_url

    return hf_hub_url(item.repo, item.source)


def _file_size_on_disk(item: PlannedFile) -> int:
    """Bytes `item` occupies after a successful transfer, `item.size` as fallback."""
    try:
        return int(os.path.getsize(item.dest))
    except OSError:
        return item.size


def _check_canceled(should_cancel: Callable[[], bool] | None, label: str) -> None:
    """Raise `DownloadCanceled` when the caller's cancel signal is up."""
    if should_cancel is not None and should_cancel():
        raise DownloadCanceled(f"Загрузка модели {label} отменена.")


def _listing_failure_message(exc: BaseException, token: str, label: str) -> str:
    """Readable refusal for a listing that could not be fetched, by `state`."""
    state = classify_repo_error(exc)
    detail = scrub(str(exc), token)
    if state == STATE_INVALID_TOKEN:
        return (
            "Токен Hugging Face отклонён (401). Проверьте, что он не отозван и имеет "
            "право на чтение репозиториев."
        )
    if state == STATE_NOT_ACCEPTED:
        return (
            "Условия использования репозитория не приняты. Откройте страницу модели "
            "на Hugging Face и примите условия, затем повторите загрузку."
        )
    if state == STATE_NOT_FOUND:
        return f"Репозиторий не найден на Hugging Face.\n{detail}"
    return f"Не удалось получить список файлов модели {label}.\n{detail}"
