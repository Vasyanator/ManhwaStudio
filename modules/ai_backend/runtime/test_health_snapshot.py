"""
File: modules/ai_backend/runtime/test_health_snapshot.py

Purpose:
Unit tests for `runtime/health_snapshot.py`: `publish` merges, `read` returns a
copy that later publications do not change, and a reader is never blocked by a
lock the publishing service holds around its own work.

Notes:
Stdlib only, like the module under test.
"""

from __future__ import annotations

import threading
import unittest

from .health_snapshot import HealthSnapshot


class HealthSnapshotTests(unittest.TestCase):
    def test_initial_fields_are_read_back(self) -> None:
        snapshot = HealthSnapshot(ready=False, last_error=None)
        self.assertEqual(snapshot.read(), {"ready": False, "last_error": None})

    def test_publish_merges_and_keeps_other_keys(self) -> None:
        snapshot = HealthSnapshot(ready=False, device="cpu", last_error=None)
        snapshot.publish(ready=True, last_error="boom")
        self.assertEqual(snapshot.read(), {"ready": True, "device": "cpu", "last_error": "boom"})

    def test_read_is_a_copy_unaffected_by_later_publications(self) -> None:
        snapshot = HealthSnapshot(ready=False)
        before = snapshot.read()
        before["ready"] = "mutated by the caller"
        snapshot.publish(ready=True)
        self.assertEqual(before, {"ready": "mutated by the caller"})
        self.assertEqual(snapshot.read(), {"ready": True})

    def test_read_does_not_wait_for_the_publishers_work_lock(self) -> None:
        # The service holds its own work lock (as a model load does) in another
        # thread; reading the snapshot must still return at once.
        work_lock = threading.Lock()
        snapshot = HealthSnapshot(ready=False)
        held = threading.Event()
        release = threading.Event()

        def hold() -> None:
            with work_lock:
                snapshot.publish(ready=False, state_note="loading")
                held.set()
                release.wait(5.0)

        holder = threading.Thread(target=hold, daemon=True)
        holder.start()
        try:
            self.assertTrue(held.wait(5.0))
            results: list[dict] = []
            reader = threading.Thread(target=lambda: results.append(snapshot.read()), daemon=True)
            reader.start()
            reader.join(2.0)
            self.assertFalse(reader.is_alive(), "read() blocked on the publisher's work lock")
            self.assertEqual(results, [{"ready": False, "state_note": "loading"}])
        finally:
            release.set()
            holder.join(5.0)


if __name__ == "__main__":
    unittest.main()
