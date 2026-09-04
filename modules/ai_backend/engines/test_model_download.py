"""
File: modules/ai_backend/engines/test_model_download.py

Purpose:
Unit tests for the shared staged-download primitive used by `inpaint/flux_fill.py`
and `watermark/service.py`.

Main responsibilities:
- verify a destination that is already on disk is not fetched again;
- verify two concurrent calls for the same destination run the transfer exactly
  once, and that the loser skips it instead of refetching;
- verify the staging file is process-private and sits next to the destination;
- verify the destination only ever appears complete: the integrity gate runs on
  the staging file, a rejected or failed transfer leaves nothing behind, and a
  present destination is never replaced by a failed attempt;
- verify calls for different destinations are not serialized against each other;
- verify `stream_response_to_file` reports cumulative bytes and tolerates a
  missing `Content-Length`, and that an appended (resumed) body counts from the
  offset it was given rather than restarting at zero;
- verify resume is OPT-IN: the default failure path still unlinks and parks
  nothing, while `resumable=True` parks the partial under the stable `.part`
  name, claims it back by an atomic rename that only one owner can win, and never
  inherits a stale pid-scoped leftover;
- verify `download_bearer_to_path` builds the `Authorization: Bearer` header from
  the token it is GIVEN, sends none for an empty token, skips a destination that
  is already on disk, forwards the caller's `verify` gate so it runs on the
  STAGED bytes before the publish, and — the cancellation contract both Hugging
  Face callers rely on — leaves neither destination nor staging file behind when
  the progress callback or the gate raises.

Notes:
The network is never involved. `download_to_path` takes the transport as a
callable and `stream_response_to_file` only needs an object with `headers` and
`iter_content`; the `download_bearer_to_path` tests install a fake `requests`
module in `sys.modules` for their duration, so the real one is never called.
"""

from __future__ import annotations

import os
import sys
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


class _FakeResponse:
    """Minimal stand-in for a streaming `requests.Response`."""

    def __init__(self, payload: bytes, *, content_length: str | None) -> None:
        self._payload = payload
        self.headers: dict[str, str] = {}
        if content_length is not None:
            self.headers["Content-Length"] = content_length

    def iter_content(self, chunk_size: int = 1 << 20):
        for start in range(0, len(self._payload), chunk_size):
            yield self._payload[start : start + chunk_size]
        # Keep-alive padding: `requests` can yield empty chunks, which must not
        # be reported as progress.
        yield b""


class StreamResponseToFileTests(unittest.TestCase):
    PAYLOAD = b"0123456789" * 300

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dest = Path(tmp.name) / "blob.bin"
        self.progress: list[tuple[int, int]] = []

    def _on_chunk(self, done: int, expected: int) -> None:
        self.progress.append((done, expected))

    def test_body_is_written_and_progress_is_cumulative(self) -> None:
        response = _FakeResponse(self.PAYLOAD, content_length=str(len(self.PAYLOAD)))
        md.stream_response_to_file(response, self.dest, self._on_chunk, chunk_size=1024)

        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)
        self.assertEqual([done for done, _ in self.progress], [1024, 2048, 3000])
        self.assertEqual({expected for _, expected in self.progress}, {len(self.PAYLOAD)})

    def test_a_missing_content_length_is_reported_as_zero(self) -> None:
        response = _FakeResponse(self.PAYLOAD, content_length=None)
        md.stream_response_to_file(response, self.dest, self._on_chunk)

        self.assertEqual(self.progress, [(len(self.PAYLOAD), 0)])

    def test_an_appended_body_counts_from_the_offset_it_was_given(self) -> None:
        # A resumed transfer must report the WHOLE file's progress. Counting from
        # zero would make an overall bar jump backwards and turn the first delta
        # a consumer measures into a nonsense transfer rate.
        self.dest.write_bytes(b"HEAD" * 250)  # 1000 bytes already on disk
        tail = b"T" * 2000
        response = _FakeResponse(tail, content_length=str(len(tail)))

        md.stream_response_to_file(
            response, self.dest, self._on_chunk, chunk_size=1000, mode="ab", initial_done=1000
        )

        self.assertEqual(self.dest.stat().st_size, 3000)
        self.assertEqual([done for done, _ in self.progress], [2000, 3000])
        # `expected` is the whole file, not just this range's length.
        self.assertEqual({expected for _, expected in self.progress}, {3000})

    def test_an_appended_body_without_a_content_length_reports_unknown(self) -> None:
        self.dest.write_bytes(b"x" * 10)
        response = _FakeResponse(b"y" * 20, content_length=None)

        md.stream_response_to_file(
            response, self.dest, self._on_chunk, mode="ab", initial_done=10
        )

        # `0` keeps meaning "the server did not say", rather than being mistaken
        # for a total that happens to equal the offset.
        self.assertEqual(self.progress, [(30, 0)])

    def test_an_unwritable_destination_is_an_explicit_error(self) -> None:
        response = _FakeResponse(self.PAYLOAD, content_length=None)
        with self.assertRaises(RuntimeError) as caught:
            md.stream_response_to_file(response, self.dest.parent / "missing" / "x.bin", self._on_chunk)

        self.assertIn("x.bin", str(caught.exception))


class _BearerRequestsModule:
    """`sys.modules['requests']` stand-in recording the headers of each GET."""

    def __init__(self, payload: bytes, *, chunk: int = 1024) -> None:  # noqa: D107
        self._payload = payload
        self._chunk = chunk
        self.calls: list[tuple[str, dict[str, str]]] = []

    def get(self, url: str, **kwargs):
        self.calls.append((url, dict(kwargs.get("headers") or {})))
        return _BearerResponse(self._payload, self._chunk)


class _BearerResponse(_FakeResponse):
    """`_FakeResponse` plus the context-manager and status surface `requests.get` has."""

    def __init__(self, payload: bytes, chunk: int) -> None:
        super().__init__(payload, content_length=str(len(payload)))
        self._chunk_hint = chunk

    def __enter__(self) -> "_BearerResponse":
        return self

    def __exit__(self, *exc_info) -> None:
        return None

    def raise_for_status(self) -> None:
        return None

    def iter_content(self, chunk_size: int = 1 << 20):
        return super().iter_content(chunk_size=self._chunk_hint)


class DownloadBearerToPathTests(unittest.TestCase):
    """The Hugging Face composition: bearer header, staging, cancel-by-raising."""

    PAYLOAD = b"weights" * 512

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dest = Path(tmp.name) / "shard.safetensors"

    def _install(self, module: _BearerRequestsModule) -> None:
        saved = sys.modules.get("requests")

        def restore() -> None:
            if saved is None:
                sys.modules.pop("requests", None)
            else:
                sys.modules["requests"] = saved

        self.addCleanup(restore)
        sys.modules["requests"] = module

    def test_the_token_becomes_a_bearer_header(self) -> None:
        fake = _BearerRequestsModule(self.PAYLOAD)
        self._install(fake)

        seen: list[tuple[int, int]] = []
        self.assertTrue(
            md.download_bearer_to_path(
                "https://example/x", self.dest, lambda d, e: seen.append((d, e)), token="tok"
            )
        )

        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)
        self.assertEqual(fake.calls[0][1].get("Authorization"), "Bearer tok")
        self.assertEqual(seen[-1][0], len(self.PAYLOAD))

    def test_an_empty_token_sends_no_authorization_header(self) -> None:
        fake = _BearerRequestsModule(self.PAYLOAD)
        self._install(fake)

        md.download_bearer_to_path("https://example/x", self.dest, lambda d, e: None, token="")

        self.assertNotIn("Authorization", fake.calls[0][1])

    def test_a_present_destination_is_not_refetched(self) -> None:
        self.dest.parent.mkdir(parents=True, exist_ok=True)
        self.dest.write_bytes(b"already here")
        fake = _BearerRequestsModule(self.PAYLOAD)
        self._install(fake)

        self.assertFalse(
            md.download_bearer_to_path("https://example/x", self.dest, lambda d, e: None, token="t")
        )
        self.assertEqual(fake.calls, [])

    def test_the_verify_gate_is_forwarded_and_runs_before_the_publish(self) -> None:
        # Without forwarding, a body that ends cleanly but SHORT is published
        # under the final name and is then indistinguishable from a complete
        # file. The gate is the caller's only chance to notice.
        fake = _BearerRequestsModule(self.PAYLOAD)
        self._install(fake)
        staged: list[int] = []

        def verify(path: Path) -> None:
            staged.append(path.stat().st_size)
            raise RuntimeError("wrong length")

        with self.assertRaises(RuntimeError):
            md.download_bearer_to_path(
                "https://example/x", self.dest, lambda d, e: None, token="t", verify=verify
            )

        self.assertEqual(staged, [len(self.PAYLOAD)])
        self.assertFalse(self.dest.exists())
        self.assertFalse(list(self.dest.parent.glob("*.part")))

    def test_a_passing_verify_publishes(self) -> None:
        fake = _BearerRequestsModule(self.PAYLOAD)
        self._install(fake)

        def verify(path: Path) -> None:
            if path.stat().st_size != len(self.PAYLOAD):
                raise RuntimeError("wrong length")

        self.assertTrue(
            md.download_bearer_to_path(
                "https://example/x", self.dest, lambda d, e: None, token="t", verify=verify
            )
        )
        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)

    def test_a_failure_unlinks_by_default_and_parks_nothing(self) -> None:
        # Resume is OPT-IN. The default must keep the semantics `flux_fill.py`
        # has always had, under BOTH staging names.
        fake = _BearerRequestsModule(self.PAYLOAD)
        self._install(fake)

        def on_chunk(done: int, expected: int) -> None:
            raise RuntimeError("dropped")

        with self.assertRaises(RuntimeError):
            md.download_bearer_to_path("https://example/x", self.dest, on_chunk, token="t")

        self.assertFalse(self.dest.exists())
        self.assertFalse(list(self.dest.parent.glob("*.part")))
        self.assertEqual(md.staged_bytes(self.dest), 0)

    def test_a_resumable_failure_parks_the_partial_under_the_stable_name(self) -> None:
        fake = _BearerRequestsModule(self.PAYLOAD, chunk=64)
        self._install(fake)
        seen: list[int] = []

        def on_chunk(done: int, expected: int) -> None:
            seen.append(done)
            if len(seen) >= 3:
                raise RuntimeError("dropped")

        with self.assertRaises(RuntimeError):
            md.download_bearer_to_path(
                "https://example/x", self.dest, on_chunk, token="t", resumable=True
            )

        self.assertFalse(self.dest.exists())
        parked = md.resumable_staging_path(self.dest)
        self.assertTrue(parked.is_file())
        self.assertEqual(parked.stat().st_size, seen[-1])
        self.assertEqual(md.staged_bytes(self.dest), seen[-1])
        # The pid-scoped name is free again: a parked partial has one owner.
        self.assertFalse(md.staging_path(self.dest).exists())

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

    def test_a_raising_progress_callback_aborts_and_leaves_nothing_behind(self) -> None:
        # This is the supported cancellation mechanism: `stream_response_to_file`
        # has no cancel hook, so the caller raises from `on_chunk` and the
        # staging file must not survive it.
        fake = _BearerRequestsModule(self.PAYLOAD)
        self._install(fake)

        class _Stop(Exception):
            pass

        def on_chunk(done: int, expected: int) -> None:
            raise _Stop()

        with self.assertRaises(_Stop):
            md.download_bearer_to_path("https://example/x", self.dest, on_chunk, token="t")

        self.assertFalse(self.dest.exists())
        self.assertFalse(list(self.dest.parent.glob("*.part")))


if __name__ == "__main__":
    unittest.main()
