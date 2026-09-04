"""
File: modules/ai_backend/inpaint/flux2_download.py

Purpose:
Acquisition of the FLUX.2 klein 9B weights from Hugging Face into
`ManhwaStudio_AI_Models/side_models/FLUX.2-klein-9B/`, behind the two IPC
methods `inpaint.flux2_klein.download.check` and `.start`. The wire contract is
`dev-docs/flux2_model_download.md`; this module is its Python half.

Model layout produced here (every path relative to `model_root()`):
    transformer/                official `transformer/*`
    text_encoder/               official `text_encoder/*`             — EXACTLY ONE
    text_encoder_uncensored/    uncensored repo minus its GGUF quants — OF THESE TWO
    tokenizer/                  official `tokenizer/*` — ALWAYS the official one
    vae/                        official `vae/*`
    scheduler/scheduler_config.json, model_index.json, LICENSE.md

The `uncensored` toggle SELECTS the encoder; it never adds a second one, so one
run downloads ~34.7 GB whichever way it is set. Both encoder directories may
still end up on disk from two runs with different settings — that is left
untouched, never re-fetched and never cleaned up.

Main responsibilities:
- build the download plan from PREFIXES resolved against the live repo listing,
  never from hard-coded shard names, so a re-sharded repo fails loudly instead of
  producing half a model;
- answer the per-repository access check with the fixed `state` taxonomy the UI
  draws three different links from;
- run the transfer outside every service lock, with two-level throttled progress,
  a free-space guard before the first byte, and cancellation that really stops
  the socket;
- publish a file only once its length matches the size the hub listing announced,
  because a body that ends CLEANLY but short raises nothing anywhere in the stack
  and would otherwise be renamed to its final name and skipped forever.

Key structures:
- `ManifestEntry` — one (repo, prefix) -> destination rule.
- `PlannedFile` — one resolved file of the plan.
- `DownloadCanceled` — typed cancellation raised at a chunk boundary.

Key functions:
- `resolve_plan()` — the pure plan builder (a listing in, a plan out).
- `is_complete_on_disk()` — presence means the ANNOUNCED SIZE, not non-empty.
- `check_access()` — the `.check` answer (`plan` is NULLABLE; see its docstring).
- `download()` — the `.start` answer.
- `model_root()` / `component_paths()`

Notes:
The Hugging Face token is a REQUEST FIELD. This module never reads it from the
environment, never persists it, never logs it and never interpolates it into an
error message; a message that must mention it says "токен" and nothing more.
Messages coming back from `huggingface_hub` are additionally scrubbed of the
token value before they leave this module.

`HfApi.model_info()` succeeds for these repositories WITHOUT a token — the
metadata of a `gated: auto` repo is public — so a returned listing proves
nothing about access. The gate is probed with `HfApi.auth_check()` instead,
which is the only call that answers "may this token download these files".
"""

from __future__ import annotations

import logging
import os
import shutil
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Iterable, Mapping, Protocol

try:
    import config as _config
except Exception:  # pragma: no cover - config is always importable in-app
    _config = None

from ..engines.model_download import discard_staging, download_bearer_to_path, staged_bytes
from ..runtime.paths import program_root

log = logging.getLogger(__name__)

# --- Repositories ---------------------------------------------------------
#: The official, gated (`gated: auto`) FLUX.2 klein 9B repository.
OFFICIAL_REPO = "black-forest-labs/FLUX.2-klein-9B"

#: The gated uncensored text encoder. Only fetched while the toggle is on.
UNCENSORED_REPO = "ponpoke/flux2-klein-9b-uncensored-text-encoder"

#: Directory under `side_models/` that receives everything.
MODEL_DIR_NAME = "FLUX.2-klein-9B"

# --- Destination subdirectories -------------------------------------------
TRANSFORMER_SUBDIR = "transformer"
TEXT_ENCODER_SUBDIR = "text_encoder"
TEXT_ENCODER_UNCENSORED_SUBDIR = "text_encoder_uncensored"
VAE_SUBDIR = "vae"

# --- Access states (`dev-docs/flux2_model_download.md` §3, FIXED) ----------
STATE_OK = "ok"
STATE_NO_TOKEN = "no_token"
STATE_INVALID_TOKEN = "invalid_token"
STATE_NOT_ACCEPTED = "not_accepted"
STATE_NOT_FOUND = "not_found"
STATE_NETWORK_ERROR = "network_error"

#: Suffixes never downloaded from either repository, whatever prefix resolves
#: them. The four `*.gguf` of the uncensored repo are llama.cpp format and this
#: engine's `transformers` loader cannot read them; the `*.jpg` are sample
#: images.
EXCLUDED_SUFFIXES = (".gguf", ".jpg", ".jpeg")

#: Exact repository-root file names never downloaded. `LICENSE.md` is NOT one of
#: them — it is an explicit manifest entry.
EXCLUDED_NAMES = ("README.md", ".gitattributes")

#: Free space demanded on top of `missing_bytes` before the first byte is
#: fetched. A model this size that fills the disk at the 34th gigabyte is a real
#: harm, and the staging `.part` file of the file in flight lives next to the
#: destination, so the margin also covers it.
FREE_SPACE_MARGIN_BYTES = 1 << 30

#: Progress frames: at most this many per second AND at least
#: `PROGRESS_MIN_BYTES` apart. `flux_fill` reports on every chunk, which at 35 GB
#: would be an IPC storm — deliberately not copied.
PROGRESS_MAX_PER_SECOND = 10.0
PROGRESS_MIN_BYTES = 4 << 20

#: The `phase` every frame of this download carries.
PROGRESS_PHASE = "download"


class ProgressCb(Protocol):
    """Two-level progress sink of the download.

    `step` / `total` are the OVERALL bytes of the transfer, which is what keeps
    every existing single-level consumer correct. The three keyword arguments
    describe the file in flight and are optional: a consumer that does not know
    them ignores them, and a preparation frame omits them.
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


class IncompleteDownload(RuntimeError):
    """A transfer ended without delivering the number of bytes the listing announced.

    Raised by the publish-time gate, so it is always raised BEFORE anything is
    published. It is a distinct type because `download()` answers it specifically:
    the staged bytes are discarded and the file is fetched once more from zero,
    which is the one recovery that covers a stale partial whose remote blob has
    changed. Any other failure is not retried.
    """


class DownloadCanceled(Exception):
    """Raised at a chunk boundary when the caller's cancel signal went up.

    The handler maps this to the dispatcher's `Interrupted`. Files already
    published stay — they are complete and were renamed atomically; the staging
    `.part` of the file in flight is removed by `download_to_path`'s failure
    path, which is what makes raising from the progress callback a safe way to
    stop a multi-gigabyte transfer.
    """


@dataclass(frozen=True)
class ManifestEntry:
    """One rule mapping repository content to a destination under `model_root()`.

    `prefix` is interpreted by shape, deliberately, so the manifest reads like
    the contract's table:
    - `""` — the whole repository (used for the uncensored encoder);
    - ending in `"/"` — a directory; every file beneath it is taken;
    - anything else — that exact file.

    `dest` is the destination path relative to `model_root()`, `"/"`-separated.
    For a directory or whole-repo entry it is the destination DIRECTORY and the
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
        """Destination of `path`, relative to `model_root()` and `"/"`-separated.

        Assumes `matches(path)`; callers filter first.
        """
        if not self.is_directory:
            return self.dest
        relative = path[len(self.prefix) :]
        return f"{self.dest}/{relative}" if self.dest else relative


#: Taken from the official repository in BOTH toggle states — everything the
#: pipeline needs except the text encoder. The `transformer/` FOLDER is the one we
#: take and the 18.157 GB root `flux-2-klein-9b.safetensors` is the same weights
#: as a single file: diffusers 0.39 loads a single file through
#: `from_single_file`, which accepts `device_map` and silently discards it, so the
#: layered offload this engine depends on would be lost.
SHARED_MANIFEST: tuple[ManifestEntry, ...] = (
    ManifestEntry(OFFICIAL_REPO, "transformer/", TRANSFORMER_SUBDIR),
    # The tokenizer ALWAYS comes from the official repo, in both toggle states:
    # the uncensored repo ships only `tokenizer.json` + `tokenizer_config.json`,
    # without `merges.txt`, `vocab.json`, `special_tokens_map.json` or
    # `added_tokens.json`, and `flux2_klein._require_component_dir` refuses a
    # tokenizer directory that fails its markers. 17 MB is not worth a second
    # failure mode.
    ManifestEntry(OFFICIAL_REPO, "tokenizer/", "tokenizer"),
    ManifestEntry(OFFICIAL_REPO, "vae/", VAE_SUBDIR),
    ManifestEntry(OFFICIAL_REPO, "scheduler/scheduler_config.json", "scheduler/scheduler_config.json"),
    ManifestEntry(OFFICIAL_REPO, "model_index.json", "model_index.json"),
    ManifestEntry(OFFICIAL_REPO, "LICENSE.md", "LICENSE.md"),
)

#: The official text encoder — the plan's encoder while the toggle is OFF.
OFFICIAL_ENCODER_MANIFEST: tuple[ManifestEntry, ...] = (
    ManifestEntry(OFFICIAL_REPO, "text_encoder/", TEXT_ENCODER_SUBDIR),
)

#: The uncensored text encoder — the plan's encoder while the toggle is ON:
#: everything that repository holds except the globally excluded files, flattened
#: into its own directory.
UNCENSORED_ENCODER_MANIFEST: tuple[ManifestEntry, ...] = (
    ManifestEntry(UNCENSORED_REPO, "", TEXT_ENCODER_UNCENSORED_SUBDIR),
)


@dataclass(frozen=True)
class PlannedFile:
    """One file of the resolved download plan.

    `label` is the destination path relative to `model_root()` and is what the
    progress frames carry; `dest` is the absolute path on disk. `size` is the
    size the repository listing announced, `0` when it announced none.
    """

    repo: str
    source: str
    dest: str
    label: str
    size: int


def model_root() -> Path:
    """Directory that receives the whole model: `<side_models>/FLUX.2-klein-9B`.

    Resolved through root `config.SIDE_MODELS_DIR` when `config` is importable
    and against `runtime.paths.program_root()` otherwise — never by counting
    parent directories here.
    """
    if _config is not None and hasattr(_config, "SIDE_MODELS_DIR"):
        return Path(_config.SIDE_MODELS_DIR) / MODEL_DIR_NAME
    return Path(program_root()) / "ManhwaStudio_AI_Models" / "side_models" / MODEL_DIR_NAME


def required_repos(uncensored: bool) -> tuple[str, ...]:
    """Repositories the current toggle state actually needs, in report order.

    Only these are checked and reported: the official one always, the uncensored
    one only while the toggle is on.
    """
    return (OFFICIAL_REPO, UNCENSORED_REPO) if uncensored else (OFFICIAL_REPO,)


def manifest_entries(uncensored: bool) -> tuple[ManifestEntry, ...]:
    """The manifest rules in force for the current toggle state.

    **The two encoder rules are EXCLUSIVE: exactly one of them is ever in a
    plan.** The toggle SELECTS the encoder, it never adds a second one, so a plan
    is ~34.7 GB in both states rather than ~51 GB with the toggle on. Everything
    else — the transformer, the tokenizer, the VAE and the configs — comes from
    the official repository either way, which is why both repositories are still
    access-checked when the toggle is on (`required_repos`).

    Both encoder directories may nevertheless end up on disk, from two runs with
    different settings. That is fine and is left alone: the missing-only filter
    never re-fetches the one already there, and nothing here ever deletes it.
    """
    encoder = UNCENSORED_ENCODER_MANIFEST if uncensored else OFFICIAL_ENCODER_MANIFEST
    return SHARED_MANIFEST + encoder


def component_paths(uncensored: bool) -> dict[str, str]:
    """Absolute component directories a finished download leaves behind.

    The three keys are the engine settings the Rust side writes back, so a
    finished download leaves a configured engine. `text_encoder` follows the
    toggle: it names the uncensored directory while the toggle is on, which is
    the same rule that decides which encoder was downloaded.
    """
    root = model_root()
    encoder = TEXT_ENCODER_UNCENSORED_SUBDIR if uncensored else TEXT_ENCODER_SUBDIR
    return {
        "transformer": str(root / TRANSFORMER_SUBDIR),
        "text_encoder": str(root / encoder),
        "vae": str(root / VAE_SUBDIR),
    }


def is_excluded(path: str) -> bool:
    """Whether the repository path `path` is one of the files never downloaded.

    Applied to every entry's resolved files, including the whole-repo one, so the
    four GGUF quants, the sample images, the README and `.gitattributes` are
    filtered no matter which rule pulled them in.
    """
    name = path.rsplit("/", 1)[-1]
    if name in EXCLUDED_NAMES:
        return True
    return any(name.lower().endswith(suffix) for suffix in EXCLUDED_SUFFIXES)


def resolve_plan(
    listings: Mapping[str, Mapping[str, int]],
    *,
    uncensored: bool,
    root: Path | str | None = None,
) -> list[PlannedFile]:
    """Resolve the manifest prefixes against live repository listings.

    `listings` maps a repository id to `{path inside the repo: size in bytes}`,
    which is exactly what `repo_listing()` returns. The result is ordered by
    destination so a run is reproducible and a resumed run continues in the same
    order.

    The manifest is expressed as PREFIXES on purpose: a repository that is
    re-sharded must not silently produce half a model.

    # Errors
    Raises `RuntimeError` when a listing for a needed repository is missing, or
    when a prefix resolves to no file at all — the latter names the prefix and
    the repository, because it means the repository layout changed under us.
    """
    base = Path(root) if root is not None else model_root()
    planned: dict[str, PlannedFile] = {}

    for entry in manifest_entries(uncensored):
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
            planned[label] = PlannedFile(
                repo=entry.repo,
                source=path,
                dest=str(base.joinpath(*label.split("/"))),
                label=label,
                size=max(0, int(listing.get(path) or 0)),
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

    Presence means the RIGHT size, not merely non-empty. A body that ended
    cleanly but short used to be published under the final name and then skipped
    by every later run — a truncated multi-gigabyte shard, permanently, surfacing
    much later as corrupt weights. Comparing the length here is also what REPAIRS
    a file some earlier run already left behind at the wrong size.

    When the listing announced no size (`size == 0`) the old non-empty rule
    stands, and the reason is logged: a missing size must not silently disable
    the check for a weights shard, but nor may it force an endless re-download.
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
            "FLUX.2 klein: %s на диске имеет размер %d Б вместо объявленных %d Б — "
            "файл будет скачан заново.",
            item.label,
            actual,
            item.size,
        )
        return False
    log.warning(
        "FLUX.2 klein: репозиторий не сообщил размер файла %s — проверка длины невозможна, "
        "файл считается готовым по признаку непустоты.",
        item.label,
    )
    return actual > 0


def missing_files(plan: Iterable[PlannedFile]) -> list[PlannedFile]:
    """The planned files that are absent, empty, or the wrong length on disk.

    Files already present at the announced size are skipped, so a re-run resumes
    at file granularity and flipping the encoder toggle downloads only the
    missing encoder. See `is_complete_on_disk` for why the length is compared.
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
def _scrub(message: str, token: str | None) -> str:
    """Remove the token value from `message`, whatever produced it.

    Belt and braces for the contract's "must never be logged": nothing in this
    module interpolates the token, but a message that came back from
    `huggingface_hub` is not ours and is scrubbed before it leaves.
    """
    if token:
        message = message.replace(token, "<токен>")
    return message


def classify_repo_error(exc: BaseException) -> str:
    """Map a `huggingface_hub` failure to one of the fixed `state` values.

    The HTTP status is consulted BEFORE the exception type, and that order is the
    contract, not an accident: on a gated repository a wrong token raises
    `GatedRepoError` with status 401, and the user must be told the token is
    wrong rather than that they failed to accept the conditions. `GatedRepoError`
    also SUBCLASSES `RepositoryNotFoundError`, so type order alone cannot
    separate 403 from 404 either.

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
    """Whether `token` may download `repo`. Returns `(state, message)`.

    `message` is empty for `ok` and carries the technical detail otherwise; only
    the `network_error` row of the contract shows it to the user, and it is
    scrubbed of the token first.

    A repository listing is NOT a proof of access: `model_info()` answers for
    these `gated: auto` repositories with no token at all. `auth_check` is the
    only call that answers the question this function asks.
    """
    from huggingface_hub import HfApi

    try:
        HfApi(token=token).auth_check(repo)
    except Exception as exc:  # noqa: BLE001 - every failure is a reportable state
        state = classify_repo_error(exc)
        log.info("FLUX.2 klein: доступ к %s — %s", repo, state)
        return state, _scrub(str(exc), token)
    return STATE_OK, ""


def repo_listing(repo: str, token: str) -> dict[str, int]:
    """`{path inside the repo: size in bytes}` for every file of `repo`.

    One metadata request, no file bytes. The token is passed through because a
    private repository would need it; these two do not, which is exactly why the
    access state is probed separately.

    # Errors
    Propagates `huggingface_hub` errors; `check_access` maps them through
    `classify_repo_error` and `download` turns them into a readable refusal.
    """
    from huggingface_hub import HfApi

    info = HfApi(token=token).model_info(repo, files_metadata=True)
    listing: dict[str, int] = {}
    for sibling in info.siblings or []:
        name = getattr(sibling, "rfilename", None)
        if not name:
            continue
        listing[str(name)] = max(0, int(getattr(sibling, "size", 0) or 0))
    return listing


def fetch_listings(repos: Iterable[str], token: str) -> dict[str, dict[str, int]]:
    """Listings of every repository in `repos`, keyed by repository id."""
    return {repo: repo_listing(repo, token) for repo in repos}


def check_access(hf_token: str, *, uncensored: bool) -> dict[str, Any]:
    """The `inpaint.flux2_klein.download.check` answer.

    Reports one `state` per repository the current toggle needs, plus the `plan`
    the button labels itself with. An EMPTY token short-circuits to `no_token`
    for every repository and makes NO network call at all.

    **`plan` is NULLABLE and a zeroed plan is never synthesised.** Access and
    listing are two different network operations: a token can pass `auth_check`
    and the file listing can still fail. Zeros beside `ok` states are
    indistinguishable from "everything is already downloaded" and were observed
    rendering a complete installation on an empty machine, so a plan that could
    not be computed is `None` and `plan_error` carries the reason (scrubbed of the
    token). `plan_error` is empty whenever `plan` is present.

    A repository the user cannot download from is NOT given a fabricated plan
    either, and gets no `plan_error`: its `state` already says what is wrong, and
    an error line repeating it would send the user to a second explanation of the
    same fact.

    # Errors
    Does not raise for an inaccessible repository or a failed listing — that is
    what the `state` taxonomy and `plan_error` are for.
    """
    token = (hf_token or "").strip()
    repos = required_repos(uncensored)

    if not token:
        return {
            "repos": {repo: {"state": STATE_NO_TOKEN, "message": ""} for repo in repos},
            "plan": None,
            "plan_error": "",
        }

    states: dict[str, dict[str, str]] = {}
    for repo in repos:
        state, message = probe_repo_access(repo, token)
        states[repo] = {"state": state, "message": message}

    plan_object: dict[str, int] | None = None
    plan_error = ""
    if all(entry["state"] == STATE_OK for entry in states.values()):
        try:
            listings = fetch_listings(repos, token)
            plan_object = plan_totals(resolve_plan(listings, uncensored=uncensored))
        except Exception as exc:  # noqa: BLE001 - reported as plan_error, not as a state
            # Deliberately NOT mapped to a repo state: access genuinely succeeded,
            # and calling this `network_error` would send the user to the wrong
            # link (accept the conditions / renew the token) for a listing that
            # merely could not be fetched.
            plan_error = _scrub(str(exc), token)
            log.warning(
                "FLUX.2 klein: доступ есть, но список файлов получить не удалось: %s",
                plan_error,
            )

    return {"repos": states, "plan": plan_object, "plan_error": plan_error}


# =====================================================================
#  Free space
# =====================================================================
def _existing_ancestor(path: Path) -> Path:
    """Nearest existing directory at or above `path`.

    The model directory usually does not exist yet on the first download, and
    `shutil.disk_usage` needs a path that does.
    """
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
        log.warning("FLUX.2 klein: не удалось узнать свободное место в %s: %s", path, exc)
        return -1


def require_free_space(root: Path, required: int) -> None:
    """Refuse the download when `root`'s filesystem cannot hold `required` bytes.

    `required` is the missing-bytes total; `FREE_SPACE_MARGIN_BYTES` is added on
    top because the staging `.part` file of the file in flight lives next to its
    destination. Checked BEFORE the first byte: discovering it at the 34th
    gigabyte is a real harm.

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
        "Недостаточно места на диске для загрузки модели FLUX.2 klein.\n"
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
    least `min_interval` seconds AND at least `min_bytes` of progress. `flux_fill`
    reports on every chunk; at 35 GB and a 1 MiB chunk that is 35 000 frames, so
    the two gates together are what keeps the IPC quiet without making the bar
    look stuck.

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
    *,
    uncensored: bool,
    progress_callback: ProgressCb | None = None,
    should_cancel: Callable[[], bool] | None = None,
) -> dict[str, Any]:
    """Fetch every missing file of the plan. Returns the `.start` answer.

    The answer is `{"paths": {...}, "downloaded_bytes": N, "skipped_files": N}`;
    `paths` are the three component directories the Rust side writes into the
    engine's settings, so a finished download leaves a configured engine.

    Runs OUTSIDE every service lock — the download lock of
    `engines.model_download` is per destination path and deliberately not a
    service lock, because a multi-gigabyte transfer must never block `health()`,
    `unload()` or an eviction callback.

    `should_cancel()` is polled before every file and at every chunk boundary.
    Files already published stay; the staging file of the one in flight is
    removed by the shared primitive.

    # Errors
    Raises `ValueError` for an empty token, `DownloadCanceled` when the caller
    canceled, and `RuntimeError` for an inaccessible repository, a manifest
    prefix that resolved to nothing, insufficient free space, or a transport
    failure.
    """
    token = (hf_token or "").strip()
    if not token:
        raise ValueError(
            "Не указан токен Hugging Face. Оба репозитория FLUX.2 klein закрыты "
            "условиями, поэтому загрузка без токена невозможна."
        )

    repos = required_repos(uncensored)
    _check_canceled(should_cancel)
    try:
        listings = fetch_listings(repos, token)
    except Exception as exc:  # noqa: BLE001 - reported as a readable refusal
        raise RuntimeError(_listing_failure_message(exc, token)) from exc

    plan = resolve_plan(listings, uncensored=uncensored)
    pending = missing_files(plan)
    skipped = len(plan) - len(pending)
    root = model_root()

    total = sum(item.size for item in pending)
    # Bytes an earlier run already parked next to their destinations are on the
    # disk already; demanding them a second time would refuse a download that
    # will actually fit.
    require_free_space(root, total - sum(staged_bytes(item.dest) for item in pending))

    throttle = ProgressThrottle()
    overall_done = 0
    downloaded = 0

    for item in pending:
        _check_canceled(should_cancel)
        on_chunk = _chunk_reporter(
            item,
            offset=overall_done,
            total=total,
            throttle=throttle,
            progress_callback=progress_callback,
            should_cancel=should_cancel,
        )
        ran = _fetch_one(item, token=token, on_chunk=on_chunk)
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

    return {
        "paths": component_paths(uncensored),
        "downloaded_bytes": downloaded,
        "skipped_files": skipped,
    }


def _chunk_reporter(
    item: PlannedFile,
    *,
    offset: int,
    total: int,
    throttle: ProgressThrottle,
    progress_callback: ProgressCb | None,
    should_cancel: Callable[[], bool] | None,
) -> Callable[[int, int], None]:
    """Per-file chunk sink: cancellation check + throttled two-level progress.

    `offset` is the overall byte count already transferred before this file, so
    the frame's `step` stays an OVERALL figure while `file_step` describes the
    file in flight.

    The returned callable RAISES `DownloadCanceled` when the caller canceled.
    That is deliberate and is the only thing that actually stops a multi-gigabyte
    transfer: `stream_response_to_file` has no cancel hook, but an exception from
    the chunk callback propagates out of the streaming loop and
    `download_to_path` removes the staging file on the way out.
    """

    def on_chunk(done: int, expected: int) -> None:
        _check_canceled(should_cancel)
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


def _fetch_one(
    item: PlannedFile,
    *,
    token: str,
    on_chunk: Callable[[int, int], None],
) -> bool:
    """Fetch one planned file, resuming what an earlier run left. Returns whether it ran.

    Resumable: a connection that dropped at 8 of 9.8 GB leaves those bytes parked
    beside the destination, and this run continues from them instead of paying for
    them twice. On a 35 GB plan over a flaky link that is the difference between a
    feature that finishes and one that never does.

    The publish-time size gate stays the last word, and its ONE recovery lives
    here: a length mismatch discards the staged bytes and refetches the file from
    zero exactly once. That covers the case the resume cannot — a stale partial
    whose remote blob has changed underneath it — without an ETag/`If-Range`
    sidecar, which the size check plus one clean retry makes unnecessary.

    # Errors
    Raises `RuntimeError` when the second, from-zero attempt is also incomplete,
    naming the file. Every other failure propagates unretried.
    """
    verify = _size_verifier(item)
    url = _file_url(item)
    try:
        return download_bearer_to_path(
            url, item.dest, on_chunk, token=token, verify=verify, resumable=True
        )
    except IncompleteDownload as first:
        log.warning(
            "FLUX.2 klein: %s — %s Повторная загрузка с нуля.",
            item.label,
            str(first).replace("\n", " "),
        )

    # The parked bytes are not merely incomplete, they are unusable: resuming from
    # them again could only reproduce the same wrong length.
    discard_staging(item.dest)
    try:
        return download_bearer_to_path(
            url, item.dest, on_chunk, token=token, verify=verify, resumable=True
        )
    except IncompleteDownload as second:
        raise RuntimeError(
            f"Не удалось полностью скачать файл модели: {item.label}\n"
            f"{second}\n"
            "Попытка повторной загрузки с нуля тоже завершилась неполным файлом — "
            "проверьте соединение или прокси и повторите позже."
        ) from second


def _size_verifier(item: PlannedFile) -> Callable[[Path], None] | None:
    """Integrity gate for `item`: the staged length must equal the announced size.

    Returns `None` when the listing announced no size, because there is then
    nothing to compare against; `is_complete_on_disk` logs that fallback on the
    planning side, so the same file is not reported twice per run.

    The gate exists because neither `stream_response_to_file` nor `requests`
    treats a SHORT body that ends cleanly as an error: a proxy or transport that
    closes without raising otherwise reaches the publish as a success, the
    truncated file is renamed to its final name, and every later run skips it.
    `download_to_path` runs this before the rename and deletes the staging file
    when it raises, so a rejected transfer leaves nothing behind.
    """
    if item.size <= 0:
        return None

    def verify(staging: Path) -> None:
        actual = int(os.path.getsize(staging))
        if actual == item.size:
            return
        raise IncompleteDownload(
            f"Файл модели скачан не полностью: {item.label}\n"
            f"Ожидалось: {item.size} Б, получено: {actual} Б."
        )

    return verify


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


def _check_canceled(should_cancel: Callable[[], bool] | None) -> None:
    """Raise `DownloadCanceled` when the caller's cancel signal is up."""
    if should_cancel is not None and should_cancel():
        raise DownloadCanceled("Загрузка модели FLUX.2 klein отменена.")


def _listing_failure_message(exc: BaseException, token: str) -> str:
    """Readable refusal for a listing that could not be fetched, by `state`."""
    state = classify_repo_error(exc)
    detail = _scrub(str(exc), token)
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
    return f"Не удалось получить список файлов модели FLUX.2 klein.\n{detail}"
