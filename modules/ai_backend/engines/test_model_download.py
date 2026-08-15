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
  missing `Content-Length`.

Notes:
Neither the network nor `requests` is involved: `download_to_path` takes the
transport as a callable, and `stream_response_to_file` only needs an object with
`headers` and `iter_content`.
"""

from __future__ import annotations

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

    def test_an_unwritable_destination_is_an_explicit_error(self) -> None:
        response = _FakeResponse(self.PAYLOAD, content_length=None)
        with self.assertRaises(RuntimeError) as caught:
            md.stream_response_to_file(response, self.dest.parent / "missing" / "x.bin", self._on_chunk)

        self.assertIn("x.bin", str(caught.exception))


if __name__ == "__main__":
    unittest.main()
