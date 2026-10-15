"""
File: modules/ai_backend/engines/test_model_download.py

Purpose:
Unit tests for the shared staged-download primitive used by `inpaint/flux_fill.py`,
`inpaint/flux2_download.py` and `watermark/service.py`.

Main responsibilities:
- verify a destination that is already on disk is not fetched again;
- verify two concurrent calls for the same destination run the transfer exactly
  once, and that the loser skips it instead of refetching;
- verify the staging file is process-private and sits next to the destination;
- verify the destination only ever appears complete: the integrity gate runs on
  the staging file, a rejected or failed transfer leaves nothing behind, and a
  present destination is never replaced by a failed attempt;
- verify calls for different destinations are not serialized against each other;
- verify `download_to_path`'s resume is OPT-IN: its default failure path still
  unlinks and parks nothing, while a resumable transfer parks the partial under
  the stable `.part` name, claims it back by an atomic rename that only one owner
  can win, and never inherits a stale pid-scoped leftover;
- verify `download_http_to_path` sends caller headers on every hop, follows an
  interstitial INSIDE each attempt (a retry repeats the confirm step and resumes
  with `Range`), refuses a follow loop, propagates a follow refusal, deletes
  bytes its `verify` rejected, and with an unknown size trusts `Content-Length`;
- verify `fetch_small_body` retries transient failures (including a short body),
  gives up after the budget, never retries a permanent status, refuses a body
  over its ceiling and stops on cancel during a backoff wait;
- verify `download_bearer_to_path` builds the `Authorization: Bearer` header from
  the token it is GIVEN (none for an empty token) with split `(15, 60)` timeouts;
  treats a destination as present only at the expected size; retries connect
  errors, timeouts, broken bodies, clean short bodies and 408 / 429 / 5xx with
  capped exponential backoff (`Retry-After` honoured, capped) while RESUMING the
  staged bytes; resets the budget after a retry that made progress; gives up
  after the budget with the bytes kept; never retries a permanent status;
  deletes the staged bytes only when they are proven wrong (overflow, a `206`
  announcing another total, a sha256 mismatch); never appends a `206` that
  starts elsewhere; never stages an answer that cannot be the file (a portal
  page, a wrong `Content-Length`, a 200 to a resume without the full length),
  keeps a partial until a from-zero restart is longer than it, measures
  progress against the furthest byte ever staged; and stops at a chunk or within 100 ms of a backoff wait when
  cancelled, parking the bytes.

Notes:
The network is never involved. `download_to_path` takes the transport as a
callable; the `download_http_to_path` / `download_bearer_to_path` tests inject a scripted `get` and
a recording `sleep`, so neither the real transport nor the real clock is used.
"""

from __future__ import annotations

import hashlib
import os
import tempfile
import threading
import time
import unittest
from pathlib import Path

from modules.ai_backend.engines import model_download as md


class DownloadToPathTests(unittest.TestCase):
    PAYLOAD = b"weights" * 4096

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.dest = self.root / "models" / "checkpoint.bin"
        self.staging: list[Path] = []

    def _fetch(self, staging: Path) -> None:
        """Write the payload slowly, so an unserialized second writer interleaves."""
        self.staging.append(staging)
        with staging.open("wb") as handle:
            for start in range(0, len(self.PAYLOAD), 4096):
                handle.write(self.PAYLOAD[start : start + 4096])
                time.sleep(0.01)

    def test_a_present_destination_is_not_fetched_again(self) -> None:
        self.dest.parent.mkdir(parents=True, exist_ok=True)
        self.dest.write_bytes(self.PAYLOAD)

        def unexpected(_staging: Path) -> None:
            raise AssertionError("the file was already on disk")

        self.assertFalse(md.download_to_path(self.dest, unexpected))

    def test_an_empty_destination_counts_as_missing(self) -> None:
        self.dest.parent.mkdir(parents=True, exist_ok=True)
        self.dest.write_bytes(b"")

        self.assertTrue(md.download_to_path(self.dest, self._fetch))
        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)

    def test_two_concurrent_calls_fetch_once_and_do_not_corrupt(self) -> None:
        errors: list[BaseException] = []
        ran: list[bool] = []

        def worker() -> None:
            try:
                ran.append(md.download_to_path(self.dest, self._fetch))
            except BaseException as exc:  # noqa: BLE001 - re-raised by the assertion below
                errors.append(exc)

        threads = [threading.Thread(target=worker) for _ in range(2)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(timeout=30)

        self.assertEqual(errors, [])
        self.assertEqual(len(self.staging), 1)
        self.assertEqual(sorted(ran), [False, True])
        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)
        self.assertEqual(list(self.dest.parent.glob("*.part")), [])

    def test_two_different_destinations_are_not_serialized(self) -> None:
        entered = threading.Barrier(2, timeout=30)

        def fetch(staging: Path) -> None:
            # Deadlocks unless both calls hold different locks.
            entered.wait()
            staging.write_bytes(self.PAYLOAD)

        errors: list[BaseException] = []

        def worker(name: str) -> None:
            try:
                md.download_to_path(self.root / name, fetch)
            except BaseException as exc:  # noqa: BLE001 - re-raised by the assertion below
                errors.append(exc)

        threads = [threading.Thread(target=worker, args=(name,)) for name in ("a.bin", "b.bin")]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(timeout=30)

        self.assertEqual(errors, [])
        self.assertTrue((self.root / "a.bin").is_file())
        self.assertTrue((self.root / "b.bin").is_file())

    def test_the_staging_file_is_process_private_and_local(self) -> None:
        self.assertTrue(md.download_to_path(self.dest, self._fetch))

        staging = self.staging[0]
        self.assertEqual(staging.parent, self.dest.parent)
        self.assertIn(str(os.getpid()), staging.name)
        self.assertTrue(staging.name.startswith(self.dest.name))
        self.assertTrue(staging.name.endswith(".part"))

    def test_the_integrity_gate_runs_before_the_destination_appears(self) -> None:
        seen: list[bytes] = []

        def verify(staging: Path) -> None:
            seen.append(staging.read_bytes())
            self.assertFalse(self.dest.exists(), "verified after publishing")
            raise RuntimeError("not a checkpoint")

        with self.assertRaises(RuntimeError):
            md.download_to_path(self.dest, self._fetch, verify=verify)

        self.assertEqual(seen, [self.PAYLOAD])
        self.assertFalse(self.dest.exists())
        self.assertFalse(self.staging[0].exists())

    def test_a_failed_transfer_leaves_no_staging_file_behind(self) -> None:
        def boom(staging: Path) -> None:
            self.staging.append(staging)
            staging.write_bytes(b"half a file")
            raise RuntimeError("transport died")

        with self.assertRaises(RuntimeError):
            md.download_to_path(self.dest, boom)

        self.assertFalse(self.staging[0].exists())
        self.assertFalse(self.dest.exists())

    def test_a_failed_retry_does_not_destroy_the_previous_file(self) -> None:
        self.assertTrue(md.download_to_path(self.dest, self._fetch))
        self.dest.unlink()

        def boom(staging: Path) -> None:
            staging.write_bytes(b"garbage")
            raise RuntimeError("transport died")

        with self.assertRaises(RuntimeError):
            md.download_to_path(self.dest, boom)
        self.assertFalse(self.dest.exists())

        self.assertTrue(md.download_to_path(self.dest, self._fetch))
        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)


class _Step:
    """One scripted server answer: a status, a body, headers, and how it ends.

    `raise_after` makes the body iterator raise that exception after yielding
    the body (a dropped connection mid-transfer); `announce` overrides the
    `Content-Length` so a body can claim more than it delivers and then end
    NORMALLY (the clean-truncation case).
    """

    def __init__(
        self,
        body: bytes = b"",
        *,
        status: int = 200,
        headers: dict[str, str] | None = None,
        announce: int | None = None,
        raise_after: BaseException | None = None,
        raise_on_get: BaseException | None = None,
    ) -> None:
        self.body = body
        self.status = status
        self.headers = dict(headers or {})
        self.headers.setdefault("Content-Length", str(len(body) if announce is None else announce))
        self.raise_after = raise_after
        self.raise_on_get = raise_on_get


class _ScriptedResponse:
    """`requests.Response` surface the primitive uses, driven by one `_Step`."""

    def __init__(self, step: _Step, chunk: int) -> None:
        self._step = step
        self._chunk = chunk
        self.status_code = step.status
        self.headers = step.headers
        # What an interstitial `follow` hook parses; latin-1 maps every byte.
        self.text = step.body.decode("latin-1")

    def __enter__(self) -> "_ScriptedResponse":
        return self

    def __exit__(self, *exc_info) -> None:
        return None

    def raise_for_status(self) -> None:
        import requests

        if self.status_code >= 400:
            raise requests.HTTPError(f"HTTP {self.status_code}")

    def iter_content(self, chunk_size: int = 1 << 20):
        body = self._step.body
        for start in range(0, len(body), self._chunk):
            yield body[start : start + self._chunk]
        if self._step.raise_after is not None:
            raise self._step.raise_after


class _ScriptedGet:
    """Injected `get`: answers from a script and records every call's kwargs."""

    def __init__(self, steps: list[_Step], *, chunk: int = 256) -> None:
        self._steps = list(steps)
        self._chunk = chunk
        self.calls: list[dict] = []
        self.urls: list[str] = []

    def __call__(self, url: str, **kwargs) -> _ScriptedResponse:
        self.calls.append(dict(kwargs))
        self.urls.append(url)
        if not self._steps:
            raise AssertionError("more requests than the script allows")
        step = self._steps.pop(0)
        if step.raise_on_get is not None:
            raise step.raise_on_get
        return _ScriptedResponse(step, self._chunk)

    @property
    def ranges(self) -> list[str | None]:
        """The `Range` header of each call, `None` when absent."""
        return [(call.get("headers") or {}).get("Range") for call in self.calls]

    @property
    def remaining(self) -> int:
        return len(self._steps)


def _partial(body: bytes, *, start: int, total: int, **kwargs) -> _Step:
    """A well-formed `206` continuing at `start` of a `total`-byte file."""
    headers = {"Content-Range": f"bytes {start}-{start + len(body) - 1}/{total}"}
    return _Step(body, status=206, headers=headers, **kwargs)


class DownloadBearerToPathTests(unittest.TestCase):
    """The Hugging Face transfer: header, retry, resume, size and hash gates, cancel."""

    PAYLOAD = bytes(range(256)) * 16  # 4096 bytes

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dest = Path(tmp.name) / "shard.safetensors"
        self.sleeps: list[float] = []
        self.progress: list[tuple[int, int]] = []
        self.retries: list[md.RetryEvent] = []

    def _run(self, get: _ScriptedGet, **overrides) -> bool:
        kwargs = dict(
            token="tok",
            expected_size=len(self.PAYLOAD),
            on_progress=lambda done, total: self.progress.append((done, total)),
            on_retry=self.retries.append,
            get=get,
            sleep=self.sleeps.append,
        )
        kwargs.update(overrides)
        return md.download_bearer_to_path("https://example/x", self.dest, **kwargs)

    def _parked(self) -> bytes:
        return md.resumable_staging_path(self.dest).read_bytes()

    # -- the plain path --------------------------------------------------
    def test_the_token_becomes_a_bearer_header_with_split_timeouts(self) -> None:
        get = _ScriptedGet([_Step(self.PAYLOAD)])

        self.assertTrue(self._run(get))

        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)
        call = get.calls[0]
        self.assertEqual(call["headers"].get("Authorization"), "Bearer tok")
        self.assertEqual(call["timeout"], (md.CONNECT_TIMEOUT_S, md.READ_TIMEOUT_S))
        self.assertEqual((md.CONNECT_TIMEOUT_S, md.READ_TIMEOUT_S), (15.0, 60.0))
        self.assertTrue(call["stream"])
        self.assertEqual(self.progress[-1], (len(self.PAYLOAD), len(self.PAYLOAD)))
        self.assertFalse(list(self.dest.parent.glob("*.part")))

    def test_an_empty_token_sends_no_authorization_header(self) -> None:
        get = _ScriptedGet([_Step(self.PAYLOAD)])

        self._run(get, token="")

        self.assertNotIn("Authorization", get.calls[0]["headers"])

    def test_a_destination_at_the_expected_size_is_not_refetched(self) -> None:
        self.dest.write_bytes(self.PAYLOAD)
        get = _ScriptedGet([])

        self.assertFalse(self._run(get))
        self.assertEqual(get.calls, [])

    def test_a_destination_at_the_wrong_size_is_refetched_and_replaced(self) -> None:
        self.dest.write_bytes(b"short")
        get = _ScriptedGet([_Step(self.PAYLOAD)])

        self.assertTrue(self._run(get))
        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)

    def test_an_unknown_size_falls_back_to_non_empty_presence(self) -> None:
        self.dest.write_bytes(b"short")
        get = _ScriptedGet([])

        self.assertFalse(self._run(get, expected_size=0))

    # -- transient failures: retry + resume ------------------------------
    def test_a_dropped_connection_is_retried_and_resumed(self) -> None:
        import requests

        get = _ScriptedGet([
            _Step(self.PAYLOAD[:1000], announce=4096, raise_after=requests.exceptions.ChunkedEncodingError("reset")),
            _partial(self.PAYLOAD[1000:], start=1000, total=4096),
        ], chunk=100)

        self.assertTrue(self._run(get))

        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)
        self.assertEqual(get.ranges, [None, "bytes=1000-"])
        self.assertEqual(len(self.retries), 1)
        event = self.retries[0]
        self.assertEqual((event.attempt, event.max_attempts), (1, 5))
        self.assertEqual(event.staged_bytes, 1000)
        self.assertEqual(event.delay_s, 2.0)
        self.assertAlmostEqual(sum(self.sleeps), 2.0)
        # Cancel is polled in 100 ms slices of the wait.
        self.assertTrue(all(step <= 0.1 + 1e-9 for step in self.sleeps))
        # Progress resumes at the staged offset instead of jumping back to zero.
        dones = [done for done, _total in self.progress]
        self.assertEqual(dones, sorted(dones))

    def test_a_clean_short_body_is_resumed(self) -> None:
        get = _ScriptedGet([
            _Step(self.PAYLOAD[:3000], announce=4096),
            _partial(self.PAYLOAD[3000:], start=3000, total=4096),
        ])

        self.assertTrue(self._run(get))
        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)
        self.assertEqual(get.ranges, [None, "bytes=3000-"])

    def test_connect_errors_timeouts_and_5xx_are_transient_and_retry_after_is_honoured(self) -> None:
        import requests

        get = _ScriptedGet([
            _Step(raise_on_get=requests.exceptions.ConnectionError("dns")),
            _Step(raise_on_get=requests.exceptions.ReadTimeout("slow")),
            _Step(status=503, headers={"Retry-After": "7"}),
            _Step(status=429, headers={"Retry-After": "600"}),
            _Step(status=408),
            _Step(self.PAYLOAD),
        ])

        self.assertTrue(self._run(get))

        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)
        self.assertEqual([event.delay_s for event in self.retries], [2.0, 4.0, 7.0, 60.0, 30.0])
        self.assertEqual([event.attempt for event in self.retries], [1, 2, 3, 4, 5])

    def test_the_budget_runs_out_without_progress_and_keeps_the_bytes(self) -> None:
        import requests

        steps = [_Step(self.PAYLOAD[:500], announce=4096, raise_after=requests.exceptions.ConnectionError("reset"))]
        steps += [_Step(status=502) for _ in range(5)]
        get = _ScriptedGet(steps)

        with self.assertRaises(md.RetriesExhaustedError) as caught:
            self._run(get)

        self.assertIsInstance(caught.exception, RuntimeError)
        self.assertNotIn("tok", str(caught.exception))
        self.assertEqual(len(get.calls), 6)
        self.assertEqual(len(self.retries), 5)
        self.assertFalse(self.dest.exists())
        # A transient failure never deletes staged bytes: the next run resumes.
        self.assertEqual(self._parked(), self.PAYLOAD[:500])

    def test_the_budget_resets_after_a_retry_that_made_progress(self) -> None:
        import requests

        reset = requests.exceptions.ConnectionError("reset")
        # Four failures without progress, then one that stages bytes (which
        # restarts the count at 1), then four more: nine failures in all, which
        # a lifetime budget of five would have refused at the sixth.
        steps = [_Step(status=500) for _ in range(4)]
        steps.append(_Step(self.PAYLOAD[:1000], announce=4096, raise_after=reset))
        steps += [_Step(status=500) for _ in range(4)]
        steps.append(_partial(self.PAYLOAD[1000:], start=1000, total=4096))
        get = _ScriptedGet(steps)

        self.assertTrue(self._run(get))

        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)
        self.assertEqual([event.attempt for event in self.retries], [1, 2, 3, 4, 1, 2, 3, 4, 5])
        self.assertEqual(get.remaining, 0)

    # -- permanent failures ----------------------------------------------
    def test_a_permanent_status_is_not_retried(self) -> None:
        import requests

        get = _ScriptedGet([_Step(status=401), _Step(self.PAYLOAD)])

        with self.assertRaises(requests.HTTPError):
            self._run(get)

        self.assertEqual(len(get.calls), 1)
        self.assertEqual(self.retries, [])
        self.assertEqual(self.sleeps, [])

    def test_a_body_longer_than_expected_deletes_the_staged_bytes(self) -> None:
        # The length header matches, the body then delivers more: proven wrong.
        get = _ScriptedGet([_Step(self.PAYLOAD + b"extra", announce=4096)])

        with self.assertRaises(md.SizeMismatchError) as caught:
            self._run(get)

        self.assertEqual(caught.exception.expected, 4096)
        self.assertEqual(len(get.calls), 1)
        self.assertFalse(self.dest.exists())
        self.assertFalse(list(self.dest.parent.glob("*.part")))
        # The user-facing text names the model file, never the pid staging file.
        self.assertIn(self.dest.name, str(caught.exception))
        self.assertNotIn(".part", str(caught.exception))

    # -- answers that cannot be the file (M1) -------------------------------
    HTML = b"<html>blocked</html>" * 10

    def _portal(self) -> _Step:
        return _Step(self.HTML, headers={"Content-Type": "text/html"})

    def test_a_portal_page_is_never_staged_or_completed_by_a_resume(self) -> None:
        # The probe scenario: a 200 block page, then the real server. The page
        # must not become a staged prefix that a `Range` resume completes.
        get = _ScriptedGet([self._portal(), _Step(self.PAYLOAD)])

        self.assertTrue(self._run(get))

        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)
        self.assertEqual(get.ranges, [None, None])
        self.assertEqual(len(self.retries), 1)
        self.assertEqual(self.retries[0].staged_bytes, 0)

    def test_an_html_page_without_a_length_is_refused(self) -> None:
        page = _Step(self.HTML, headers={"Content-Type": "text/html; charset=utf-8"})
        page.headers.pop("Content-Length")
        get = _ScriptedGet([page, _Step(self.PAYLOAD)])

        self.assertTrue(self._run(get))
        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)

    def test_a_portal_answer_to_a_resume_leaves_the_partial_untouched(self) -> None:
        md.resumable_staging_path(self.dest).write_bytes(self.PAYLOAD[:3000])
        get = _ScriptedGet([self._portal() for _ in range(6)])

        with self.assertRaises(md.RetriesExhaustedError):
            self._run(get)

        self.assertEqual(self._parked(), self.PAYLOAD[:3000])
        self.assertEqual(get.ranges, ["bytes=3000-"] * 6)

    def test_a_200_to_a_resume_without_a_length_is_refused(self) -> None:
        md.resumable_staging_path(self.dest).write_bytes(self.PAYLOAD[:3000])
        bare = _Step(self.PAYLOAD)
        bare.headers.pop("Content-Length")
        get = _ScriptedGet([bare, _partial(self.PAYLOAD[3000:], start=3000, total=4096)])

        self.assertTrue(self._run(get))
        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)
        self.assertEqual(get.ranges, ["bytes=3000-", "bytes=3000-"])

    def test_a_206_whose_length_does_not_end_the_file_is_refused(self) -> None:
        md.resumable_staging_path(self.dest).write_bytes(self.PAYLOAD[:1000])
        wrong = _partial(self.PAYLOAD[1000:2000], start=1000, total=4096)
        get = _ScriptedGet([wrong, _partial(self.PAYLOAD[1000:], start=1000, total=4096)])

        self.assertTrue(self._run(get))
        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)
        self.assertEqual(len(self.retries), 1)

    def test_a_restart_that_fails_early_keeps_the_longer_partial(self) -> None:
        import requests

        md.resumable_staging_path(self.dest).write_bytes(self.PAYLOAD[:2000])
        # The server ignores the range (full-length 200) and drops at 1000
        # bytes: shorter than the partial, so the partial must survive.
        dropped = _Step(self.PAYLOAD[:1000], announce=4096, raise_after=requests.ConnectionError("reset"))
        get = _ScriptedGet([dropped, _partial(self.PAYLOAD[2000:], start=2000, total=4096)])

        self.assertTrue(self._run(get))

        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)
        self.assertEqual(get.ranges, ["bytes=2000-", "bytes=2000-"])
        self.assertEqual(self.retries[0].staged_bytes, 2000)
        self.assertFalse(list(self.dest.parent.glob("*.restart")))

    def test_a_restart_that_passes_the_partial_replaces_it(self) -> None:
        import requests

        md.resumable_staging_path(self.dest).write_bytes(b"?" * 1000)
        dropped = _Step(self.PAYLOAD[:3000], announce=4096, raise_after=requests.ConnectionError("reset"))
        get = _ScriptedGet([dropped, _partial(self.PAYLOAD[3000:], start=3000, total=4096)])

        self.assertTrue(self._run(get))

        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)
        self.assertEqual(get.ranges, ["bytes=1000-", "bytes=3000-"])

    def test_progress_is_measured_against_the_furthest_byte_ever_staged(self) -> None:
        import requests

        # 1000 bytes staged, then the server keeps refusing the range (416, a
        # restart from zero) and dropping at 500: never beyond the high-water
        # mark, so the budget runs out instead of resetting forever.
        def drop(at: int) -> _Step:
            return _Step(self.PAYLOAD[:at], announce=4096, raise_after=requests.ConnectionError("reset"))

        steps = [drop(1000)]
        for _ in range(5):
            steps += [_Step(status=416), drop(500)]
        get = _ScriptedGet(steps)

        with self.assertRaises(md.RetriesExhaustedError):
            self._run(get)

        self.assertEqual([event.attempt for event in self.retries], [1, 2, 3, 4, 5])
        self.assertEqual(get.remaining, 0)

    def test_a_206_announcing_another_total_deletes_the_staged_bytes(self) -> None:
        md.resumable_staging_path(self.dest).write_bytes(self.PAYLOAD[:1000])
        get = _ScriptedGet([_partial(self.PAYLOAD[1000:], start=1000, total=9999)])

        with self.assertRaises(md.SizeMismatchError) as caught:
            self._run(get)

        self.assertEqual(caught.exception.actual, 9999)
        self.assertFalse(list(self.dest.parent.glob("*.part")))

    def test_a_sha256_mismatch_deletes_the_staged_bytes(self) -> None:
        get = _ScriptedGet([_Step(self.PAYLOAD)])

        with self.assertRaises(md.HashMismatchError):
            self._run(get, expected_sha256="0" * 64)

        self.assertFalse(self.dest.exists())
        self.assertFalse(list(self.dest.parent.glob("*.part")))

    def test_a_matching_sha256_publishes(self) -> None:
        get = _ScriptedGet([_Step(self.PAYLOAD)])

        self.assertTrue(self._run(get, expected_sha256=hashlib.sha256(self.PAYLOAD).hexdigest().upper()))
        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)

    # -- resume answers ---------------------------------------------------
    def test_a_parked_partial_is_resumed_with_range(self) -> None:
        md.resumable_staging_path(self.dest).write_bytes(self.PAYLOAD[:1000])
        get = _ScriptedGet([_partial(self.PAYLOAD[1000:], start=1000, total=4096)])

        self.assertTrue(self._run(get))
        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)
        self.assertEqual(get.ranges, ["bytes=1000-"])
        self.assertEqual(self.progress[0][0], 1000 + 256)

    def test_a_206_starting_elsewhere_is_never_appended(self) -> None:
        md.resumable_staging_path(self.dest).write_bytes(self.PAYLOAD[:1000])
        get = _ScriptedGet([
            _partial(self.PAYLOAD[500:], start=500, total=4096),
            _Step(self.PAYLOAD),
        ])

        self.assertTrue(self._run(get))
        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)
        self.assertEqual(get.ranges, ["bytes=1000-", None])
        self.assertEqual(self.sleeps, [])

    def test_a_server_ignoring_the_range_rewrites_from_zero(self) -> None:
        md.resumable_staging_path(self.dest).write_bytes(b"?" * 1000)
        get = _ScriptedGet([_Step(self.PAYLOAD)])

        self.assertTrue(self._run(get))
        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)

    def test_a_416_discards_and_refetches_at_once(self) -> None:
        md.resumable_staging_path(self.dest).write_bytes(b"?" * 1000)
        get = _ScriptedGet([_Step(status=416), _Step(self.PAYLOAD)])

        self.assertTrue(self._run(get))
        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)
        self.assertEqual(get.ranges, ["bytes=1000-", None])
        self.assertEqual(self.retries, [])

    def test_leftovers_longer_than_expected_are_dropped_before_any_request(self) -> None:
        md.resumable_staging_path(self.dest).write_bytes(b"?" * 5000)
        get = _ScriptedGet([_Step(self.PAYLOAD)])

        self.assertTrue(self._run(get))
        self.assertEqual(get.ranges, [None])

    def test_a_complete_staged_file_is_published_without_a_request(self) -> None:
        md.resumable_staging_path(self.dest).write_bytes(self.PAYLOAD)
        get = _ScriptedGet([])

        self.assertTrue(self._run(get))
        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)

    # -- cancellation -----------------------------------------------------
    def test_a_cancel_during_the_backoff_wait_stops_and_parks(self) -> None:
        get = _ScriptedGet([_Step(self.PAYLOAD[:700], announce=4096), _Step(self.PAYLOAD)])

        def should_cancel() -> bool:
            return len(self.sleeps) >= 3

        with self.assertRaises(md.DownloadCancelled) as caught:
            self._run(get, should_cancel=should_cancel)

        self.assertNotIsInstance(caught.exception, RuntimeError)
        # Stopped three 100 ms slices into a 2 s wait, never reaching the retry.
        self.assertEqual(len(self.sleeps), 3)
        self.assertEqual(len(get.calls), 1)
        self.assertEqual(self._parked(), self.PAYLOAD[:700])

    def test_a_cancel_during_the_transfer_stops_at_a_chunk_and_parks(self) -> None:
        get = _ScriptedGet([_Step(self.PAYLOAD)])

        with self.assertRaises(md.DownloadCancelled):
            self._run(get, should_cancel=lambda: len(self.progress) >= 2)

        self.assertFalse(self.dest.exists())
        self.assertEqual(self._parked(), self.PAYLOAD[:512])

    def test_a_raising_progress_callback_aborts_and_parks(self) -> None:
        class _Stop(Exception):
            pass

        def on_progress(done: int, total: int) -> None:
            raise _Stop()

        get = _ScriptedGet([_Step(self.PAYLOAD)])
        with self.assertRaises(_Stop):
            self._run(get, on_progress=on_progress)

        self.assertFalse(self.dest.exists())
        self.assertEqual(self._parked(), self.PAYLOAD[:256])
        # The pid-scoped name is free again: a parked partial has one owner.
        self.assertFalse(md.staging_path(self.dest).exists())

    # -- policy and labels -------------------------------------------------
    def test_backoff_is_exponential_and_capped(self) -> None:
        policy = md.RetryPolicy()
        self.assertEqual([policy.delay_for(n, None) for n in range(1, 7)], [2.0, 4.0, 8.0, 16.0, 30.0, 30.0])
        self.assertEqual(policy.delay_for(1, 90.0), 60.0)

    def test_the_retry_label_names_the_wait_the_attempt_and_the_file(self) -> None:
        event = md.RetryEvent(attempt=2, max_attempts=5, delay_s=4.0, staged_bytes=10, reason="HTTP 503")
        label = md.retry_progress_label("vae/model.safetensors", event)
        self.assertIn("4", label)
        self.assertIn("2/5", label)
        self.assertTrue(label.endswith("vae/model.safetensors"))


class DownloadHttpToPathTests(unittest.TestCase):
    """The generic transfer's seams: extra headers, the interstitial hook, `verify`."""

    PAYLOAD = b"PK\x03\x04" + bytes(range(256)) * 8
    PAGE = b"<html><form><input name=confirm value=abc></form></html>"

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dest = Path(tmp.name) / "weights.pth"
        self.sleeps: list[float] = []

    def _run(self, get: _ScriptedGet, **overrides) -> bool:
        kwargs = dict(
            expected_size=len(self.PAYLOAD),
            on_progress=lambda _done, _total: None,
            get=get,
            sleep=self.sleeps.append,
        )
        kwargs.update(overrides)
        return md.download_http_to_path("https://example/first", self.dest, **kwargs)

    @staticmethod
    def _confirm_follow(response, hop: int) -> str | None:
        if "text/html" not in response.headers.get("Content-Type", ""):
            return None
        return f"https://example/confirmed?hop={hop}"

    def _page(self) -> _Step:
        return _Step(self.PAGE, headers={"Content-Type": "text/html"})

    def test_extra_headers_are_sent_on_every_hop_without_authorization(self) -> None:
        get = _ScriptedGet([self._page(), _Step(self.PAYLOAD)])

        self.assertTrue(self._run(get, headers={"User-Agent": "UA/1"}, follow=self._confirm_follow))

        self.assertEqual(get.urls, ["https://example/first", "https://example/confirmed?hop=0"])
        for call in get.calls:
            self.assertEqual(call["headers"].get("User-Agent"), "UA/1")
            self.assertNotIn("Authorization", call["headers"])
        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)

    def test_a_retry_repeats_the_confirm_step_and_resumes_with_range(self) -> None:
        half = len(self.PAYLOAD) // 2
        import requests

        get = _ScriptedGet([
            self._page(),
            _Step(self.PAYLOAD[:half], announce=len(self.PAYLOAD), raise_after=requests.ConnectionError("reset")),
            self._page(),
            _partial(self.PAYLOAD[half:], start=half, total=len(self.PAYLOAD)),
        ])

        self.assertTrue(self._run(get, follow=self._confirm_follow))

        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)
        self.assertEqual(get.ranges, [None, None, f"bytes={half}-", f"bytes={half}-"])
        self.assertAlmostEqual(sum(self.sleeps), md.RetryPolicy().delay_for(1, None))

    def test_a_follow_that_never_ends_is_a_permanent_error(self) -> None:
        get = _ScriptedGet([self._page() for _ in range(md._MAX_FOLLOW_HOPS + 1)])

        with self.assertRaises(md.DownloadError):
            self._run(get, follow=self._confirm_follow)

        self.assertEqual(get.remaining, 0)
        self.assertFalse(self.dest.exists())
        self.assertEqual(self.sleeps, [])

    def test_a_follow_refusal_propagates_unchanged(self) -> None:
        class _Refused(RuntimeError):
            pass

        def refuse(_response, hop: int) -> str | None:
            raise _Refused(f"hop {hop}")

        with self.assertRaises(_Refused):
            self._run(_ScriptedGet([_Step(self.PAYLOAD)]), follow=refuse)

    def test_a_verify_rejection_deletes_the_bytes_instead_of_parking_them(self) -> None:
        def reject(_staging: Path) -> None:
            raise RuntimeError("not a checkpoint")

        with self.assertRaises(RuntimeError):
            self._run(_ScriptedGet([_Step(self.PAYLOAD)]), verify=reject)

        self.assertFalse(self.dest.exists())
        self.assertEqual(md.staged_bytes(self.dest), 0)
        self.assertFalse(list(self.dest.parent.glob("*.part")))

    def test_an_unknown_size_trusts_the_announced_length(self) -> None:
        import requests

        get = _ScriptedGet([
            _Step(self.PAYLOAD[:100], announce=len(self.PAYLOAD), raise_after=requests.ConnectionError("x")),
            _partial(self.PAYLOAD[100:], start=100, total=len(self.PAYLOAD)),
        ])

        self.assertTrue(self._run(get, expected_size=0))
        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)


class FetchSmallBodyTests(unittest.TestCase):
    """The in-memory GET for catalogs: same retry rules, no staging."""

    BODY = b'[{"filename": "a.pth"}]'

    def setUp(self) -> None:
        self.sleeps: list[float] = []
        self.retries: list[md.RetryEvent] = []

    def _fetch(self, get: _ScriptedGet, **overrides) -> bytes:
        kwargs = dict(label="catalog", get=get, sleep=self.sleeps.append, on_retry=self.retries.append)
        kwargs.update(overrides)
        return md.fetch_small_body("https://example/catalog", **kwargs)

    def test_a_transient_failure_is_retried_and_the_body_returned(self) -> None:
        import requests

        get = _ScriptedGet([
            _Step(raise_on_get=requests.ConnectionError("dns")),
            _Step(self.BODY[:5], announce=len(self.BODY)),
            _Step(self.BODY),
        ])

        self.assertEqual(self._fetch(get, headers={"User-Agent": "UA"}), self.BODY)

        self.assertEqual([event.attempt for event in self.retries], [1, 2])
        self.assertEqual(get.calls[0]["timeout"], (md.CONNECT_TIMEOUT_S, md.READ_TIMEOUT_S))
        self.assertEqual(get.calls[-1]["headers"], {"User-Agent": "UA"})

    def test_the_budget_runs_out(self) -> None:
        get = _ScriptedGet([_Step(status=503) for _ in range(6)])

        with self.assertRaises(md.RetriesExhaustedError):
            self._fetch(get)
        self.assertEqual(len(self.retries), 5)

    def test_a_permanent_status_is_not_retried(self) -> None:
        import requests

        with self.assertRaises(requests.HTTPError):
            self._fetch(_ScriptedGet([_Step(status=404)]))
        self.assertEqual(self.sleeps, [])

    def test_a_body_over_the_ceiling_is_refused(self) -> None:
        with self.assertRaises(md.DownloadError):
            self._fetch(_ScriptedGet([_Step(b"x" * 1000)]), max_bytes=100)
        self.assertEqual(self.sleeps, [])

    def test_a_cancel_during_the_backoff_wait_stops(self) -> None:
        flag = {"cancel": False}

        def sleep(seconds: float) -> None:
            self.sleeps.append(seconds)
            flag["cancel"] = True

        with self.assertRaises(md.DownloadCancelled):
            self._fetch(_ScriptedGet([_Step(status=503)]), sleep=sleep, should_cancel=lambda: flag["cancel"])
        self.assertEqual(len(self.sleeps), 1)


class StagingHelperTests(unittest.TestCase):
    """The resumable-staging claim, park and discard helpers."""

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dest = Path(tmp.name) / "shard.safetensors"

    def test_claiming_a_parked_partial_is_a_rename_so_only_one_owner_wins(self) -> None:
        staging = md.staging_path(self.dest)
        parked = md.resumable_staging_path(self.dest)
        self.dest.parent.mkdir(parents=True, exist_ok=True)
        parked.write_bytes(b"partial")

        md._claim_staged_bytes(staging, parked)
        self.assertEqual(staging.read_bytes(), b"partial")
        self.assertFalse(parked.exists())

        # A second claimant finds nothing and is left with no staging file at
        # all, rather than inheriting bytes it has no claim to.
        other = self.dest.with_name(self.dest.name + ".999999.part")
        other.write_bytes(b"stale leftover")
        md._claim_staged_bytes(other, parked)
        self.assertFalse(other.exists())

    def test_discard_staging_removes_both_names(self) -> None:
        self.dest.parent.mkdir(parents=True, exist_ok=True)
        md.staging_path(self.dest).write_bytes(b"a")
        md.resumable_staging_path(self.dest).write_bytes(b"b")

        md.discard_staging(self.dest)

        self.assertFalse(md.staging_path(self.dest).exists())
        self.assertFalse(md.resumable_staging_path(self.dest).exists())
        self.assertEqual(md.staged_bytes(self.dest), 0)


if __name__ == "__main__":
    unittest.main()
