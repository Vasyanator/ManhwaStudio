"""
File: modules/ai_backend/inpaint/_hf_download_test_fakes.py

Purpose:
Test-only stand-ins for the network seams of a backend-side Hugging Face
download (`hf_gated_download.py` over `../engines/model_download.py`), shared
by `test_hf_gated_download.py` and `test_flux3_download.py` so there is one
implementation of each fake. Imported by tests only, never by runtime code;
the leading underscore keeps pytest from collecting it.

Key structures:
- `_FakeResponse` / `_FakeRequests` — a `requests`-shaped transport that
  records the headers it was given and can announce a length the body does
  not deliver (the clean-truncation case);
- `_RangeResponse` / `_RangeRequests` — a transport that honours, ignores or
  refuses `Range`, or ends its first ranged answer early;
- `_FakeRequestsModule` — installs a transport into the shared primitive's
  two seams (`model_download._default_get`, `model_download._default_sleep`)
  for a `with` block, recording the backoff waits instead of sleeping;
- `_Sibling` / `_Info` — a fake `model_info()` listing;
- `_FakeHfApi` — swaps `huggingface_hub.HfApi` for a fake class for a block.

Key functions:
- `_http_error()` — the real `huggingface_hub` error type carrying a status;
- `_token_recording_api()` — a fake `HfApi` class that answers like a gated
  repository and records the token of every call.

Notes:
No network, no files: everything here is in memory. Every patch is undone on
`__exit__`, so a failing test cannot leak a fake into the next one.
"""

from __future__ import annotations

from typing import Any, Callable, Iterator

from modules.ai_backend.engines import model_download as md


class _Sibling:
    """One entry of a fake `model_info().siblings` list."""

    def __init__(self, rfilename: str, size: int) -> None:
        self.rfilename = rfilename
        self.size = size


class _Info:
    """Fake `model_info()` result carrying only the siblings the module reads."""

    def __init__(self, listing: dict[str, int]) -> None:
        self.siblings = [_Sibling(name, size) for name, size in sorted(listing.items())]


def _http_error(status: int, kind: str) -> Exception:
    """Build the real `huggingface_hub` error `kind` carrying HTTP `status`.

    `kind` is `"gated"` (`GatedRepoError`), `"not_found"`
    (`RepositoryNotFoundError`) or `"http"` (`HfHubHTTPError`).
    """
    import requests
    from huggingface_hub.errors import (
        GatedRepoError,
        HfHubHTTPError,
        RepositoryNotFoundError,
    )

    response = requests.Response()
    response.status_code = status
    classes = {
        "gated": GatedRepoError,
        "not_found": RepositoryNotFoundError,
        "http": HfHubHTTPError,
    }
    return classes[kind](f"{status} Client Error.", response=response)


def _token_recording_api(
    seen: list[tuple[str, str, str | None]],
    *,
    valid_token: str | None,
    listings: dict[str, dict[str, int]],
) -> Any:
    """Build a fake `HfApi` class that records the token every call carried.

    It answers the way the real hub does for a GATED repository: 401 to no
    token and to a wrong one. `valid_token` is the only token accepted; `None`
    means no token is valid anywhere. Each call appends `(call, repo, token)`
    to `seen`, `call` being `"auth"` or `"info"`.
    """

    def answer(call: str, repo: str, token: str | None) -> None:
        seen.append((call, repo, token))
        if token is None or token != valid_token:
            raise _http_error(401, "gated")

    class _Api:
        def __init__(self, token: str | None = None) -> None:
            self._token = token

        def auth_check(self, repo: str) -> None:
            answer("auth", repo, self._token)

        def model_info(self, repo: str, files_metadata: bool = False) -> _Info:
            answer("info", repo, self._token)
            return _Info(listings[repo])

    return _Api


class _FakeHfApi:
    """`huggingface_hub.HfApi` patch installing a fake class for the duration."""

    def __init__(self, api: Any) -> None:
        self._api = api
        self._saved: Any = None

    def __enter__(self) -> Any:
        import huggingface_hub

        self._saved = huggingface_hub.HfApi
        huggingface_hub.HfApi = self._api  # type: ignore[assignment]
        return self._api

    def __exit__(self, *exc_info: Any) -> None:
        import huggingface_hub

        huggingface_hub.HfApi = self._saved  # type: ignore[assignment]


class _FakeResponse:
    """Minimal `requests` response: a `Content-Length` and a chunk iterator.

    `announced` lets a test advertise a length the body does not deliver and then
    END NORMALLY — the clean-truncation case no transport error covers.
    """

    def __init__(self, payload: bytes, chunk: int, announced: int | None = None) -> None:
        self._payload = payload
        self._chunk = chunk
        length = len(payload) if announced is None else announced
        self.headers = {"Content-Length": str(length)}
        self.closed = False

    def __enter__(self) -> "_FakeResponse":
        return self

    def __exit__(self, *exc_info: Any) -> None:
        self.closed = True

    def raise_for_status(self) -> None:
        return None

    def iter_content(self, chunk_size: int = 1) -> Iterator[bytes]:
        for start in range(0, len(self._payload), self._chunk):
            yield self._payload[start : start + self._chunk]


class _FakeRequests:
    """Stand-in for the `requests` module, recording the headers it was given.

    `payload_for(url)` decides the body of each answer; `calls` lists every
    `(url, headers)` pair in order.
    """

    def __init__(
        self,
        payload_for: Callable[[str], bytes],
        chunk: int,
        announced: int | None = None,
    ) -> None:
        self._payload_for = payload_for
        self._chunk = chunk
        self._announced = announced
        self.calls: list[tuple[str, dict[str, str]]] = []

    def get(self, url: str, **kwargs: Any) -> _FakeResponse:
        self.calls.append((url, dict(kwargs.get("headers") or {})))
        return _FakeResponse(self._payload_for(url), self._chunk, self._announced)


class _FakeRequestsModule:
    """Installs a fake transport into the shared primitive's seams for a block.

    `model_download._default_get` becomes the fake's `get`, and
    `model_download._default_sleep` records the backoff waits in `sleeps`
    instead of sleeping, so a retry costs no wall-clock time.
    """

    def __init__(self, fake: Any) -> None:
        self._fake = fake
        self._saved: Any = None
        self.sleeps: list[float] = []

    def __enter__(self) -> Any:
        self._saved = (md._default_get, md._default_sleep)
        md._default_get = self._fake.get  # type: ignore[assignment]
        md._default_sleep = self.sleeps.append  # type: ignore[assignment]
        self._fake.sleeps = self.sleeps
        return self._fake

    def __exit__(self, *exc_info: Any) -> None:
        md._default_get, md._default_sleep = self._saved  # type: ignore[assignment]


class _RangeResponse:
    """Response that knows its own status, for exercising the resume answers.

    A `206` carries `Content-Range: bytes <start>-<end>/<total>`, which the
    primitive checks before it appends anything.
    """

    def __init__(
        self,
        body: bytes,
        chunk: int,
        status: int,
        *,
        start: int = 0,
        total: int | None = None,
    ) -> None:
        self._body = body
        self._chunk = chunk
        self.status_code = status
        self.headers = {"Content-Length": str(len(body))}
        if status == 206:
            whole = total if total is not None else start + len(body)
            self.headers["Content-Range"] = f"bytes {start}-{start + len(body) - 1}/{whole}"

    def __enter__(self) -> "_RangeResponse":
        return self

    def __exit__(self, *exc_info: Any) -> None:
        return None

    def raise_for_status(self) -> None:
        if self.status_code >= 400:
            raise RuntimeError(f"HTTP {self.status_code}")

    def iter_content(self, chunk_size: int = 1) -> Iterator[bytes]:
        for start in range(0, len(self._body), self._chunk):
            yield self._body[start : start + self._chunk]


class _RangeRequests:
    """`requests` stand-in that honours (or deliberately mishandles) `Range`.

    `on_range` picks which legal answer the server gives to a range request:
    `"206"` resumes properly, `"200"` ignores the range and sends the whole body,
    `"416"` refuses it, and `"short_once"` resumes but its FIRST ranged answer
    stops ten bytes early WITHOUT raising — the clean-truncation case — while
    later ones complete.
    """

    def __init__(self, full: bytes, chunk: int = 1024, on_range: str = "206") -> None:
        self.full = full
        self.chunk = chunk
        self.on_range = on_range
        self.calls: list[dict[str, str]] = []
        self.sleeps: list[float] = []
        self._short_sent = False

    def get(self, url: str, **kwargs: Any) -> _RangeResponse:
        headers = dict(kwargs.get("headers") or {})
        self.calls.append(headers)
        span = headers.get("Range")
        if span is None:
            return _RangeResponse(self.full, self.chunk, 200)
        start = int(span.split("=", 1)[1].split("-", 1)[0])
        if self.on_range == "416":
            return _RangeResponse(b"", self.chunk, 416)
        if self.on_range == "200":
            return _RangeResponse(self.full, self.chunk, 200)
        rest = self.full[start:]
        if self.on_range == "short_once" and not self._short_sent:
            self._short_sent = True
            # Announces the whole remainder, delivers less, ends normally.
            response = _RangeResponse(rest[:-10], self.chunk, 206, start=start, total=len(self.full))
            response.headers["Content-Length"] = str(len(rest))
            return response
        return _RangeResponse(rest, self.chunk, 206, start=start)

    @property
    def ranged(self) -> list[str]:
        """The `Range` values actually sent, in order."""
        return [headers["Range"] for headers in self.calls if "Range" in headers]
