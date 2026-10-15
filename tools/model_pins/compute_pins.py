#!/usr/bin/env python3
"""Download every pinned model source once and record its exact size and sha256.

File: tools/model_pins/compute_pins.py

Purpose:
    Produce the pin values (byte size, sha256, resolved commit, zip-member digests) that the
    application's pinned download catalogs need, from the real upstream bytes. It is an offline
    developer tool: nothing in ``src/`` or ``crates/`` imports it, and it never enters the
    runtime path.

Usage::

    python3 -I tools/model_pins/compute_pins.py --sources tools/model_pins/sources.json \
        --scratch <dir outside the repo> --out pins.json [--snippets snippets.txt] \
        [--only <source id> ...] [-v]

Source kinds (``sources.json``, see ``MODULE_README.md`` for the schema):
    ``url``        one file; optional ``expected_sha256`` and ``zip_members`` (sha256, size and
                   md5 of members, md5 cross-checked against an expected value).
    ``gdrive``     a Google Drive file id, including the >100 MB confirm-interstitial flow.
    ``hf_repo``    a Hugging Face model repo: ``revision`` resolved to a 40-hex commit, the
                   recursive tree filtered by ``allow_patterns`` (fnmatch, ``*`` crosses ``/``,
                   as ``huggingface_hub`` matches) or exact ``paths``; every file is
                   cross-checked against the tree (LFS sha256, or git blob sha1 for small files).
    ``url_manifest`` a Surya-style directory: ``<base>/manifest.json`` plus its ``files`` list.
    GitHub release URLs are additionally cross-checked against the release API's asset
    ``size`` / ``digest`` (``digest`` is null for older assets; that is recorded, not an error).

Key functions:
    fetch_file()      resumable (HTTP Range) download into ``<dest>.part`` with retries.
    process_source()  one source -> one result entry (rows + checks), never raises for a
                      network or data failure of that source.
    run()             the whole run: sources -> pins.json (+ snippets), written after each source.

Network contract (PROJECT_RULES "Network downloads"):
    transient failures (connect/DNS/reset, timeout, short body, HTTP 408/429/5xx) retry with
    capped exponential backoff (``Retry-After`` honoured up to 60 s), resuming from the staged
    bytes; the attempt budget resets after a retry that made progress; other 4xx, overflow and
    local disk errors are permanent. Staged bytes are dropped only when proven wrong (overflow, a
    ``416`` whose total disagrees). ``urllib`` has one socket timeout, used for connect and read.

Security notes:
    Downloaded files are untrusted data: this tool only hashes them and reads zip member bytes.
    It never executes, imports or unpickles them. Remote-supplied relative paths (HF tree, Surya
    manifest) are validated so they cannot escape the scratch directory. The scratch directory
    must lie outside the repository (refused otherwise), so no download can land in the tree.

Dependencies: Python standard library only.
"""

from __future__ import annotations

import argparse
import fnmatch
import hashlib
import http.client
import json
import logging
import re
import socket
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import zipfile
from collections.abc import Callable
from dataclasses import dataclass, field
from http.cookiejar import CookieJar
from pathlib import Path
from typing import Any, Protocol

log = logging.getLogger("compute_pins")

#: Repository root (this file lives at ``tools/model_pins/``).
REPO_ROOT = Path(__file__).resolve().parents[2]
HF_ENDPOINT = "https://huggingface.co"
GITHUB_API = "https://api.github.com"
DRIVE_URL = "https://drive.usercontent.google.com/download?id={file_id}&export=download"
USER_AGENT = "ManhwaStudio-model-pins/1"
#: Socket timeout in seconds; ``urllib`` applies it to the connect and to every read.
TIMEOUT_S = 60.0
CHUNK = 1 << 20
#: Upper bound for small bodies read into memory (API JSON, Drive interstitial HTML).
SMALL_BODY_LIMIT = 8 << 20
PROGRESS_STEP = 256 << 20
TRANSIENT_STATUS = frozenset({408, 429, 500, 502, 503, 504})
HEX40 = re.compile(r"^[0-9a-f]{40}$")
HEX64 = re.compile(r"^[0-9a-f]{64}$")
GITHUB_RELEASE_URL = re.compile(r"^https://github\.com/([^/]+)/([^/]+)/releases/download/([^/]+)/([^/?#]+)$")
SAFE_ID = re.compile(r"[^A-Za-z0-9._-]")


class Response(Protocol):
    """The part of ``http.client.HTTPResponse`` this tool uses (so tests can fake it)."""

    status: int
    headers: Any

    def read(self, amt: int = ...) -> bytes: ...

    def close(self) -> None: ...


#: ``opener(request, timeout_s) -> Response``; raises ``urllib.error.HTTPError`` for non-2xx.
Opener = Callable[[urllib.request.Request, float], Response]


class PinError(Exception):
    """A permanent failure of one source (bad status, overflow, mismatch, malformed data)."""


class HttpStatusError(PinError):
    """A non-transient HTTP status; ``code`` and ``headers`` let callers special-case 416."""

    def __init__(self, code: int, url: str, headers: Any) -> None:
        super().__init__(f"HTTP {code} for {url}")
        self.code = code
        self.headers = headers


class TransientError(Exception):
    """A retryable failure; ``retry_after`` is the server's ``Retry-After`` in seconds, if any."""

    def __init__(self, message: str, retry_after: float | None = None) -> None:
        super().__init__(message)
        self.retry_after = retry_after


class HtmlInterstitial(Exception):
    """An HTML page arrived where a binary payload was required (Google Drive confirm form)."""

    def __init__(self, url: str, html: str) -> None:
        super().__init__(f"HTML page instead of a file at {url}")
        self.url = url
        self.html = html


@dataclass(frozen=True)
class RetryPolicy:
    """Retry budget and backoff: ``base * 2**(attempt-1)`` capped at ``max_delay_s``."""

    max_attempts_without_progress: int = 5
    base_delay_s: float = 2.0
    max_delay_s: float = 30.0
    max_retry_after_s: float = 60.0

    def delay(self, attempt: int, retry_after: float | None) -> float:
        """Seconds to wait before retry number ``attempt`` (1-based)."""
        if retry_after is not None:
            return min(max(retry_after, 0.0), self.max_retry_after_s)
        return min(self.base_delay_s * (2 ** (attempt - 1)), self.max_delay_s)


@dataclass
class Net:
    """Injected network side: the opener, the sleep and the retry policy."""

    opener: Opener
    sleep: Callable[[float], None] = time.sleep
    policy: RetryPolicy = field(default_factory=RetryPolicy)


@dataclass(frozen=True)
class FileDigest:
    """Size plus the digests of one file; ``git_sha1`` is the git blob id (HF non-LFS oid)."""

    size: int
    sha256: str
    md5: str
    git_sha1: str


def default_opener() -> Opener:
    """A ``urllib`` opener with a cookie jar (Drive's confirm flow sets a cookie)."""
    built = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(CookieJar()))

    def open_request(request: urllib.request.Request, timeout: float) -> Response:
        return built.open(request, timeout=timeout)

    return open_request


def _retry_after(headers: Any) -> float | None:
    """Parse a numeric ``Retry-After`` header; HTTP-date forms fall back to the backoff."""
    value = headers.get("Retry-After") if headers is not None else None
    if value is None:
        return None
    try:
        return float(value)
    except ValueError:
        log.debug("non-numeric Retry-After %r ignored", value)
        return None


def open_url(net: Net, url: str, headers: dict[str, str] | None = None) -> Response:
    """Open ``url``, classifying failures into ``TransientError`` / ``HttpStatusError``."""
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT, **(headers or {})})
    try:
        return net.opener(request, TIMEOUT_S)
    except urllib.error.HTTPError as exc:
        if exc.code in TRANSIENT_STATUS:
            raise TransientError(f"HTTP {exc.code} for {url}", _retry_after(exc.headers)) from exc
        raise HttpStatusError(exc.code, url, exc.headers) from exc
    except (urllib.error.URLError, TimeoutError, ConnectionError, http.client.HTTPException) as exc:
        raise TransientError(f"{type(exc).__name__}: {exc} ({url})") from exc


def with_retries(net: Net, what: str, op: Callable[[], Any], progress: Callable[[], int]) -> Any:
    """Run ``op`` until it succeeds, retrying ``TransientError`` per ``net.policy``.

    ``progress()`` reports bytes staged so far; a retry that made progress resets the budget.
    Raises ``PinError`` once the budget is spent; other exceptions propagate unchanged.
    """
    attempt = 0
    last = progress()
    while True:
        try:
            return op()
        except TransientError as exc:
            now = progress()
            if now > last:
                attempt = 0
                last = now
            attempt += 1
            if attempt > net.policy.max_attempts_without_progress:
                raise PinError(f"{what}: giving up after {attempt - 1} retries: {exc}") from exc
            delay = net.policy.delay(attempt, exc.retry_after)
            log.warning("%s: transient failure (%s); retry %d/%d in %.1f s, staged %d bytes",
                        what, exc, attempt, net.policy.max_attempts_without_progress, delay, now)
            net.sleep(delay)


def _read_chunk(resp: Response, url: str) -> bytes:
    """One body read; a dropped connection or truncated body becomes a ``TransientError``."""
    try:
        return resp.read(CHUNK)
    except (TimeoutError, ConnectionError, http.client.HTTPException, socket.error) as exc:
        raise TransientError(f"read failed: {type(exc).__name__}: {exc} ({url})") from exc


def get_bytes(net: Net, url: str, limit: int = SMALL_BODY_LIMIT) -> bytes:
    """GET a small body into memory (retried); ``PinError`` if it exceeds ``limit``."""

    def attempt() -> bytes:
        resp = open_url(net, url)
        try:
            parts: list[bytes] = []
            total = 0
            while chunk := _read_chunk(resp, url):
                total += len(chunk)
                if total > limit:
                    raise PinError(f"body of {url} exceeds {limit} bytes")
                parts.append(chunk)
            length = resp.headers.get("Content-Length")
            if length is not None and length.isdigit() and int(length) != total:
                raise TransientError(f"short body: {total} of {length} bytes ({url})")
            return b"".join(parts)
        finally:
            resp.close()

    return with_retries(net, url, attempt, lambda: 0)


def get_json(net: Net, url: str) -> Any:
    """GET and parse a JSON document; ``PinError`` on malformed JSON."""
    body = get_bytes(net, url)
    try:
        return json.loads(body)
    except (json.JSONDecodeError, UnicodeDecodeError) as exc:
        raise PinError(f"malformed JSON from {url}: {exc}") from exc


def parse_content_range(value: str | None) -> tuple[int | None, int | None]:
    """``bytes a-b/N`` -> ``(a, N)``; ``bytes */N`` -> ``(None, N)``; ``N`` is ``None`` for ``*``."""
    if not value:
        return None, None
    match = re.fullmatch(r"\s*bytes\s+(?:(\d+)-\d+|\*)/(\d+|\*)\s*", value)
    if match is None:
        raise PinError(f"malformed Content-Range {value!r}")
    start = int(match.group(1)) if match.group(1) is not None else None
    total = int(match.group(2)) if match.group(2) != "*" else None
    return start, total


def fetch_file(net: Net, url: str, dest: Path, *, reject_html: bool = False) -> None:
    """Download ``url`` to ``dest`` through ``<dest>.part``, resuming with HTTP Range.

    An existing ``dest`` counts as complete (a previous run finished it; its digest is
    recomputed by the caller anyway). Raises ``PinError`` for permanent failures,
    ``HtmlInterstitial`` when ``reject_html`` and the server answers with an HTML page.
    """
    if dest.is_file():
        log.info("already staged: %s", dest)
        return
    dest.parent.mkdir(parents=True, exist_ok=True)
    part = dest.with_name(dest.name + ".part")

    def staged() -> int:
        return part.stat().st_size if part.exists() else 0

    def attempt() -> None:
        offset = staged()
        headers = {"Range": f"bytes={offset}-"} if offset else {}
        try:
            resp = open_url(net, url, headers)
        except HttpStatusError as exc:
            if exc.code == 416 and offset:
                _, total = parse_content_range(exc.headers.get("Content-Range") if exc.headers else None)
                if total == offset:
                    log.info("%s: staged part already complete (%d bytes)", url, offset)
                    return
                # The staged part is longer than (or inconsistent with) the remote file:
                # proven wrong, so it is dropped and the download restarts.
                log.warning("%s: 416 with total %s for staged %d bytes; discarding part", url, total, offset)
                part.unlink()
                raise TransientError("staged part rejected by server (416)") from exc
            raise
        try:
            content_type = (resp.headers.get("Content-Type") or "").lower()
            if reject_html and "text/html" in content_type:
                body = resp.read(SMALL_BODY_LIMIT)
                raise HtmlInterstitial(url, body.decode("utf-8", "replace"))
            if resp.status == 206:
                start, total = parse_content_range(resp.headers.get("Content-Range"))
                if start is None:
                    raise PinError(f"206 without a byte range from {url}")
                if start > offset:
                    log.warning("%s: 206 starts at %d past staged %d; discarding part", url, start, offset)
                    part.unlink()
                    raise TransientError("206 start beyond staged bytes")
                if start < offset:
                    log.info("%s: 206 starts at %d before staged %d; truncating", url, start, offset)
                    with part.open("r+b") as handle:
                        handle.truncate(start)
                written, mode, expected = start, "ab", total
            elif resp.status == 200:
                if offset:
                    log.info("%s: server ignored Range; restarting from 0", url)
                length = resp.headers.get("Content-Length")
                written, mode = 0, "wb"
                expected = int(length) if length is not None and length.isdigit() else None
            else:
                raise PinError(f"unexpected HTTP {resp.status} for {url}")
            next_report = (written // PROGRESS_STEP + 1) * PROGRESS_STEP
            with part.open(mode) as handle:
                while chunk := _read_chunk(resp, url):
                    handle.write(chunk)
                    written += len(chunk)
                    if expected is not None and written > expected:
                        handle.close()
                        part.unlink()
                        raise PinError(f"{url}: body overflows the announced {expected} bytes")
                    if written >= next_report:
                        log.info("%s: %d MiB of %s", dest.name, written >> 20,
                                 f"{expected >> 20} MiB" if expected is not None else "?")
                        next_report += PROGRESS_STEP
            if expected is not None and written < expected:
                raise TransientError(f"short body: {written} of {expected} bytes")
        finally:
            resp.close()

    with_retries(net, url, attempt, staged)
    part.replace(dest)


def hash_file(path: Path) -> FileDigest:
    """sha256, md5 and git-blob sha1 of ``path`` in one pass."""
    size = path.stat().st_size
    sha256 = hashlib.sha256()
    md5 = hashlib.md5(usedforsecurity=False)
    git = hashlib.sha1(f"blob {size}\0".encode(), usedforsecurity=False)
    with path.open("rb") as handle:
        while chunk := handle.read(CHUNK):
            sha256.update(chunk)
            md5.update(chunk)
            git.update(chunk)
    return FileDigest(size, sha256.hexdigest(), md5.hexdigest(), git.hexdigest())


def safe_relpath(value: Any) -> Path:
    """Validate a remote-supplied relative path; ``PinError`` if it could escape its root."""
    if not isinstance(value, str) or not value or "\\" in value or "\0" in value:
        raise PinError(f"unsafe remote path {value!r}")
    parts = value.split("/")
    if value.startswith("/") or any(p in ("", ".", "..") for p in parts) or re.match(r"^[A-Za-z]:", value):
        raise PinError(f"unsafe remote path {value!r}")
    return Path(*parts)


def check_scratch(scratch: Path, repo_root: Path = REPO_ROOT) -> Path:
    """Resolve ``scratch``; ``PinError`` if it is the repository or lies inside it."""
    resolved = scratch.expanduser().resolve()
    root = repo_root.resolve()
    if resolved == root or root in resolved.parents:
        raise PinError(f"scratch {resolved} is inside the repository {root}; choose a path outside it")
    return resolved


@dataclass
class Context:
    """Per-run state: network, scratch root, URL -> staged-file cache, GitHub release cache."""

    net: Net
    scratch: Path
    fetched: dict[str, Path] = field(default_factory=dict)
    github_releases: dict[tuple[str, str, str], dict[str, Any] | str] = field(default_factory=dict)


def _download(ctx: Context, url: str, dest: Path, *, reject_html: bool = False) -> Path:
    """Fetch ``url`` once per run; a repeated URL reuses the first staged file."""
    cached = ctx.fetched.get(url)
    if cached is not None:
        return cached
    log.info("downloading %s", url)
    fetch_file(ctx.net, url, dest, reject_html=reject_html)
    ctx.fetched[url] = dest
    return dest


def github_asset_check(ctx: Context, url: str, digest: FileDigest) -> dict[str, Any] | None:
    """Cross-check a GitHub release asset against the release API (``None`` if not GitHub)."""
    match = GITHUB_RELEASE_URL.match(url)
    if match is None:
        return None
    owner, repo, tag, name = match.groups()
    key = (owner, repo, urllib.parse.unquote(tag))
    if key not in ctx.github_releases:
        api = f"{GITHUB_API}/repos/{owner}/{repo}/releases/tags/{tag}"
        try:
            release = get_json(ctx.net, api)
            ctx.github_releases[key] = {a["name"]: a for a in release.get("assets", []) if isinstance(a, dict) and "name" in a}
        except PinError as exc:
            log.warning("GitHub release lookup failed for %s: %s", api, exc)
            ctx.github_releases[key] = str(exc)
    assets = ctx.github_releases[key]
    if isinstance(assets, str):
        return {"github_api_error": assets}
    asset = assets.get(urllib.parse.unquote(name))
    if asset is None:
        return {"github_api_error": f"asset {name} not listed in release {tag}"}
    result: dict[str, Any] = {"github_size": asset.get("size"), "github_size_match": asset.get("size") == digest.size,
                              "github_digest": asset.get("digest")}
    gh_digest = asset.get("digest")
    if isinstance(gh_digest, str) and gh_digest.startswith("sha256:"):
        result["github_digest_match"] = gh_digest.removeprefix("sha256:") == digest.sha256
    return result


def _file_row(ctx: Context, url: str, path: str, digest: FileDigest) -> dict[str, Any]:
    """The common row of one downloaded file, with GitHub checks when applicable."""
    row: dict[str, Any] = {"path": path, "url": url, "size": digest.size, "sha256": digest.sha256, "checks": {}}
    gh = github_asset_check(ctx, url, digest)
    if gh is not None:
        row["checks"].update(gh)
    return row


def zip_member_rows(archive: Path, base_row: dict[str, Any], members: Any) -> list[dict[str, Any]]:
    """One row per requested zip member: its size, sha256 and md5 (md5 cross-checked).

    ``members`` is ``"*"`` (every file member) or a list of ``{"member", "md5"?}``.
    Raises ``PinError`` for a non-zip archive or a missing member.
    """
    try:
        with zipfile.ZipFile(archive) as zf:
            if members == "*":
                wanted = [{"member": i.filename} for i in zf.infolist() if not i.is_dir()]
            elif isinstance(members, list):
                wanted = members
            else:
                raise PinError(f"zip_members must be '*' or a list, got {members!r}")
            rows = []
            for spec in wanted:
                name = spec["member"]
                try:
                    info = zf.getinfo(name)
                except KeyError as exc:
                    listed = ", ".join(zf.namelist()[:20])
                    raise PinError(f"member {name!r} not in {archive.name} (has: {listed})") from exc
                sha256 = hashlib.sha256()
                md5 = hashlib.md5(usedforsecurity=False)
                size = 0
                with zf.open(info) as handle:
                    while chunk := handle.read(CHUNK):
                        sha256.update(chunk)
                        md5.update(chunk)
                        size += len(chunk)
                row = {**base_row, "checks": dict(base_row["checks"]), "member": name, "member_size": size,
                       "member_sha256": sha256.hexdigest(), "member_md5": md5.hexdigest()}
                expected_md5 = spec.get("md5")
                if expected_md5 is not None:
                    row["checks"]["member_md5_expected"] = expected_md5
                    row["checks"]["member_md5_match"] = expected_md5 == row["member_md5"]
                rows.append(row)
            return rows
    except zipfile.BadZipFile as exc:
        raise PinError(f"{archive.name} is not a valid zip: {exc}") from exc


def _source_dir(ctx: Context, source_id: str) -> Path:
    return ctx.scratch / SAFE_ID.sub("_", source_id)


def _url_source(ctx: Context, source: dict[str, Any]) -> dict[str, Any]:
    url = source["url"]
    name = urllib.parse.unquote(url.rstrip("/").rsplit("/", 1)[-1].split("?", 1)[0])
    staged = _download(ctx, url, _source_dir(ctx, source["id"]) / safe_relpath(name))
    digest = hash_file(staged)
    row = _file_row(ctx, url, name, digest)
    expected = source.get("expected_sha256")
    if expected is not None:
        row["checks"]["expected_sha256"] = expected
        row["checks"]["expected_sha256_match"] = expected == digest.sha256
    members = source.get("zip_members")
    rows = zip_member_rows(staged, row, members) if members is not None else [row]
    return {"rows": rows}


def drive_confirm_url(base_url: str, html: str) -> str:
    """Drive's confirm URL from its interstitial form (mirrors ``watermark/service.py``)."""
    confirm = re.search(r'name="confirm"\s+value="([^"]+)"', html)
    uuid = re.search(r'name="uuid"\s+value="([^"]+)"', html)
    url = f"{base_url}&confirm={urllib.parse.quote(confirm.group(1) if confirm else 't', safe='')}"
    if uuid:
        url = f"{url}&uuid={urllib.parse.quote(uuid.group(1), safe='')}"
    return url


def _gdrive_source(ctx: Context, source: dict[str, Any]) -> dict[str, Any]:
    file_id = source["file_id"]
    url = DRIVE_URL.format(file_id=urllib.parse.quote(file_id, safe=""))
    dest = _source_dir(ctx, source["id"]) / safe_relpath(source["file_name"])
    try:
        _download(ctx, url, dest, reject_html=True)
    except HtmlInterstitial as page:
        confirm = drive_confirm_url(url, page.html)
        log.info("Drive interstitial for %s; following the confirm form", file_id)
        try:
            fetch_file(ctx.net, confirm, dest, reject_html=True)
        except HtmlInterstitial as again:
            raise PinError(f"Drive still returned HTML after confirm for {file_id} (quota or removed file)") from again
        ctx.fetched[url] = dest
    digest = hash_file(dest)
    row = _file_row(ctx, url, source["file_name"], digest)
    approx = source.get("approx_size")
    if approx is not None:
        row["checks"]["code_approx_size"] = approx
    return {"rows": [row]}


def _hf_tree(ctx: Context, repo: str, commit: str) -> list[dict[str, Any]]:
    """The recursive file tree of ``repo`` at ``commit``, following ``Link: rel=next`` pages."""
    url: str | None = f"{HF_ENDPOINT}/api/models/{repo}/tree/{commit}?recursive=1"
    entries: list[dict[str, Any]] = []
    while url is not None:
        resp_body, link = _get_with_link(ctx.net, url)
        try:
            page = json.loads(resp_body)
        except (json.JSONDecodeError, UnicodeDecodeError) as exc:
            raise PinError(f"malformed tree JSON from {url}: {exc}") from exc
        if not isinstance(page, list):
            raise PinError(f"tree listing of {repo} is not a list")
        entries.extend(e for e in page if isinstance(e, dict) and e.get("type") == "file")
        url = link
    return entries


def _get_with_link(net: Net, url: str) -> tuple[bytes, str | None]:
    """GET a small body and return it with the ``rel="next"`` URL of its ``Link`` header."""
    holder: dict[str, str | None] = {"next": None}

    def attempt() -> bytes:
        resp = open_url(net, url)
        try:
            body = resp.read(SMALL_BODY_LIMIT + 1)
            if len(body) > SMALL_BODY_LIMIT:
                raise PinError(f"tree page {url} too large")
            match = re.search(r'<([^>]+)>\s*;\s*rel="next"', resp.headers.get("Link") or "")
            holder["next"] = match.group(1) if match else None
            return body
        finally:
            resp.close()

    body = with_retries(net, url, attempt, lambda: 0)
    return body, holder["next"]


def _hf_source(ctx: Context, source: dict[str, Any]) -> dict[str, Any]:
    repo = source["repo"]
    revision = source.get("revision", "main")
    info = get_json(ctx.net, f"{HF_ENDPOINT}/api/models/{repo}/revision/{urllib.parse.quote(revision, safe='')}")
    commit = info.get("sha") if isinstance(info, dict) else None
    if not isinstance(commit, str) or not HEX40.match(commit):
        raise PinError(f"{repo}@{revision}: revision API returned no 40-hex sha")
    tree = _hf_tree(ctx, repo, commit)
    patterns = source.get("allow_patterns")
    paths = source.get("paths")
    if paths is not None:
        selected = [e for e in tree if e.get("path") in set(paths)]
        missing = set(paths) - {e.get("path") for e in selected}
        if missing:
            raise PinError(f"{repo}@{commit}: paths not in tree: {sorted(missing)}")
    elif patterns is not None:
        selected = [e for e in tree if any(fnmatch.fnmatchcase(str(e.get("path")), p) for p in patterns)]
    else:
        raise PinError(f"{source['id']}: hf_repo needs 'paths' or 'allow_patterns'")
    if not selected:
        raise PinError(f"{repo}@{commit}: no file matched")
    rows = []
    root = _source_dir(ctx, f"hf/{repo}/{commit}")
    for entry in sorted(selected, key=lambda e: str(e["path"])):
        rel = safe_relpath(entry["path"])
        url = f"{HF_ENDPOINT}/{repo}/resolve/{commit}/{urllib.parse.quote(entry['path'])}"
        digest = hash_file(_download(ctx, url, root / rel))
        row = _file_row(ctx, url, entry["path"], digest)
        row["checks"]["tree_size"] = entry.get("size")
        row["checks"]["tree_size_match"] = entry.get("size") == digest.size
        lfs = entry.get("lfs")
        if isinstance(lfs, dict) and isinstance(lfs.get("oid"), str):
            row["checks"]["tree_lfs_sha256_match"] = lfs["oid"] == digest.sha256
        else:
            row["checks"]["tree_git_blob_match"] = entry.get("oid") == digest.git_sha1
        rows.append(row)
    expected = source.get("expected_sha256")
    if expected is not None:
        for row in rows:
            row["checks"]["expected_sha256"] = expected
            row["checks"]["expected_sha256_match"] = expected == row["sha256"]
    return {"repo": repo, "revision": revision, "commit": commit, "rows": rows}


def _manifest_source(ctx: Context, source: dict[str, Any]) -> dict[str, Any]:
    base = f"{source['base_url'].rstrip('/')}/{source['path'].strip('/')}"
    root = _source_dir(ctx, source["id"])
    manifest_url = f"{base}/manifest.json"
    manifest_path = _download(ctx, manifest_url, root / "manifest.json")
    try:
        manifest = json.loads(manifest_path.read_bytes())
    except (json.JSONDecodeError, UnicodeDecodeError) as exc:
        raise PinError(f"malformed manifest {manifest_url}: {exc}") from exc
    files = manifest.get("files") if isinstance(manifest, dict) else None
    if not isinstance(files, list) or not files:
        raise PinError(f"{manifest_url}: no 'files' list")
    rows = [_file_row(ctx, manifest_url, "manifest.json", hash_file(manifest_path))]
    for name in files:
        rel = safe_relpath(name)
        url = f"{base}/{urllib.parse.quote(name)}"
        rows.append(_file_row(ctx, url, name, hash_file(_download(ctx, url, root / rel))))
    return {"base_url": base, "rows": rows}


HANDLERS: dict[str, Callable[[Context, dict[str, Any]], dict[str, Any]]] = {
    "url": _url_source,
    "gdrive": _gdrive_source,
    "hf_repo": _hf_source,
    "url_manifest": _manifest_source,
}


def _all_checks_pass(rows: list[dict[str, Any]]) -> bool:
    return all(v is not False for row in rows for k, v in row["checks"].items() if k.endswith("_match"))


def process_source(ctx: Context, source: dict[str, Any]) -> dict[str, Any]:
    """Run one source; returns ``{id, kind, status, ...}`` with status ok / mismatch / failed."""
    source_id = source.get("id", "?")
    kind = source.get("kind")
    entry: dict[str, Any] = {"id": source_id, "kind": kind}
    if "note" in source:
        entry["note"] = source["note"]
    handler = HANDLERS.get(kind) if isinstance(kind, str) else None
    if handler is None:
        return {**entry, "status": "failed", "error": f"unknown source kind {kind!r}"}
    started = time.monotonic()
    try:
        result = handler(ctx, source)
    except PinError as exc:
        log.error("source %s failed: %s", source_id, exc)
        return {**entry, "status": "failed", "error": str(exc)}
    except OSError as exc:
        # Local disk errors (scratch full, permissions) are permanent for this source.
        log.error("source %s failed on local I/O: %s", source_id, exc)
        return {**entry, "status": "failed", "error": f"local I/O: {exc}"}
    status = "ok" if _all_checks_pass(result["rows"]) else "mismatch"
    log.info("source %s: %s (%d rows, %.0f s)", source_id, status, len(result["rows"]), time.monotonic() - started)
    return {**entry, "status": status, **result}


def _rust_int(value: int) -> str:
    return f"{value:_}"


def render_snippets(results: list[dict[str, Any]]) -> str:
    """Ready-to-paste Rust (``ExternalFile``), JSON (ORT-row style) and Python lines per source."""
    out: list[str] = []
    for res in results:
        if res.get("status") == "failed":
            out.append(f"# {res['id']}: FAILED - {res.get('error')}\n")
            continue
        origin = f"{res['repo']} @ {res['commit']}" if "commit" in res else res.get("base_url", res["rows"][0]["url"])
        out.append(f"# ===== {res['id']} [{res['status']}] {origin}")
        out.append("## Rust")
        for row in res["rows"]:
            out.append(f'ExternalFile {{ path: "{row["path"]}", size: {_rust_int(row["size"])}, sha256: "{row["sha256"]}" }},')
            if "member" in row:
                out.append(f'    // member "{row["member"]}": size {_rust_int(row["member_size"])}, '
                           f'sha256 "{row["member_sha256"]}", md5 "{row["member_md5"]}"')
        out.append("## JSON")
        for row in res["rows"]:
            item = {"url": row["url"], "sha256": row["sha256"], "size": row["size"]}
            if "member" in row:
                item.update(member=row["member"], member_size=row["member_size"], member_sha256=row["member_sha256"])
            out.append(json.dumps(item))
        out.append("## Python")
        for row in res["rows"]:
            out.append(f'"{row.get("member", row["path"])}": ({row.get("member_size", row["size"])}, '
                       f'"{row.get("member_sha256", row["sha256"])}"),')
        out.append("")
    return "\n".join(out) + "\n"


def _write_atomic(path: Path, text: str) -> None:
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(text, encoding="utf-8")
    tmp.replace(path)


def run(sources: list[dict[str, Any]], ctx: Context, out: Path, snippets: Path | None) -> list[dict[str, Any]]:
    """Process every source in order, rewriting ``out`` (and ``snippets``) after each one."""
    results: list[dict[str, Any]] = []
    for source in sources:
        results.append(process_source(ctx, source))
        document = {"tool": "tools/model_pins/compute_pins.py", "generated_unix": int(time.time()), "sources": results}
        _write_atomic(out, json.dumps(document, indent=2) + "\n")
        if snippets is not None:
            _write_atomic(snippets, render_snippets(results))
    return results


def load_sources(path: Path) -> list[dict[str, Any]]:
    """Read ``sources.json`` (``{"sources": [...]}``); ``PinError`` on a malformed file or duplicate id."""
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise PinError(f"cannot read sources {path}: {exc}") from exc
    sources = document.get("sources") if isinstance(document, dict) else None
    if not isinstance(sources, list) or not all(isinstance(s, dict) and "id" in s for s in sources):
        raise PinError(f"{path}: expected {{'sources': [{{'id': ...}}, ...]}}")
    ids = [s["id"] for s in sources]
    duplicates = {i for i in ids if ids.count(i) > 1}
    if duplicates:
        raise PinError(f"{path}: duplicate source ids {sorted(duplicates)}")
    return sources


def main(argv: list[str] | None = None) -> int:
    """CLI entry. Exit 0 = every source ok, 1 = a source failed or mismatched, 2 = usage error."""
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--sources", type=Path, required=True)
    parser.add_argument("--scratch", type=Path, required=True, help="download directory OUTSIDE the repository")
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--snippets", type=Path, default=None)
    parser.add_argument("--only", action="append", default=None, help="process only this source id (repeatable)")
    parser.add_argument("-v", "--verbose", action="store_true")
    args = parser.parse_args(argv)
    logging.basicConfig(level=logging.DEBUG if args.verbose else logging.INFO,
                        format="%(asctime)s %(levelname)s %(name)s: %(message)s")
    try:
        scratch = check_scratch(args.scratch)
        sources = load_sources(args.sources)
    except PinError as exc:
        log.error("%s", exc)
        return 2
    if args.only:
        unknown = set(args.only) - {s["id"] for s in sources}
        if unknown:
            log.error("unknown --only ids: %s", sorted(unknown))
            return 2
        sources = [s for s in sources if s["id"] in set(args.only)]
    scratch.mkdir(parents=True, exist_ok=True)
    ctx = Context(net=Net(opener=default_opener()), scratch=scratch)
    results = run(sources, ctx, args.out, args.snippets)
    if args.snippets is None:
        sys.stdout.write(render_snippets(results))
    bad = [r["id"] for r in results if r["status"] != "ok"]
    total = sum(p.stat().st_size for p in set(ctx.fetched.values()) if p.is_file())
    log.info("done: %d sources, %d not ok %s, %.2f GiB staged", len(results), len(bad), bad, total / (1 << 30))
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
