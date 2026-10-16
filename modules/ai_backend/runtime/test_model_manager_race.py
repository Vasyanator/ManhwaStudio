"""
File: modules/ai_backend/runtime/test_model_manager_race.py

Purpose:
Reproduces the lease race of STATE S-058 with two real threads and two model
keys against a stub service that follows the documented lease protocol
(`runtime/model_manager.py` file header): a lease that found key A resident
goes stale while another request switches the service to key B, and the
service then reloads A under that stale lease.

Main responsibilities:
- after the race, the manager registers A resident (it is counted by the cap)
  and B not resident, matching what the service actually holds;
- A is still evictable: its entry kept the unload callback.

Notes:
- `_StubService` is the protocol's reference shape (lease before the service
  lock; load, then `mark_loaded` unconditionally under the lock; `release` in a
  `finally`), not a copy of any one service. The cross-service form of the same
  race lives in `inpaint/test_lease_protocol.py`.
- The service lock is an `RLock` so the main thread can run the switching
  request while it holds the lock the second thread is blocked on — the shape
  of the reviewer's probe.
"""

from __future__ import annotations

import threading
import unittest
from typing import Callable

from modules.ai_backend.runtime.model_manager import SmallModelManager

#: Upper bound for every wait in this file; the race itself takes milliseconds.
_TIMEOUT_S = 10.0


class _StubService:
    """A one-model service: holds at most one key, swaps it on demand."""

    def __init__(self, manager: SmallModelManager) -> None:
        self._manager = manager
        self.lock = threading.RLock()
        self.active_key: str | None = None
        self.loads: list[str] = []

    def run(self, key: str, *, after_lease: Callable[[], None] | None = None) -> None:
        """One request for `key`; `after_lease` runs between the lease and the service lock."""
        lease = self._manager.begin_model_use(key, unload_callback=lambda: self.unload_key(key))
        try:
            if after_lease is not None:
                after_lease()
            with self.lock:
                try:
                    self._ensure_locked(key)
                except Exception:
                    if lease.needs_load:
                        lease.mark_load_failed()
                    raise
                lease.mark_loaded(unload_callback=lambda: self.unload_key(key))
        finally:
            lease.release()

    def unload_key(self, key: str) -> bool:
        """Eviction callback: drop the model when it is still `key`."""
        with self.lock:
            if self.active_key != key:
                return False
            self.active_key = None
            self._manager.mark_unloaded(key)
            return True

    def _ensure_locked(self, key: str) -> None:
        if self.active_key == key:
            return
        previous = self.active_key
        if previous is not None:
            self.active_key = None
            self._manager.mark_unloaded(previous)
        self.loads.append(key)
        self.active_key = key


class LeaseRaceTests(unittest.TestCase):
    def test_a_model_reloaded_under_a_stale_lease_is_registered(self) -> None:
        # Two slots: B is loaded while A is still in use (by the stale lease).
        manager = SmallModelManager(max_loaded_models=2)
        service = _StubService(manager)
        service.run("a")  # A resident and registered.

        leased = threading.Event()
        failures: list[BaseException] = []

        def second_request() -> None:
            try:
                service.run("a", after_lease=leased.set)
            except BaseException as exc:  # reported to the main thread below
                failures.append(exc)

        with service.lock:
            worker = threading.Thread(target=second_request, name="lease-race-a")
            worker.start()
            # The second request holds a lease that found A resident and now
            # waits for the service lock.
            self.assertTrue(leased.wait(_TIMEOUT_S))
            # Meanwhile another request switches the service to B, which drops
            # A from residency (`mark_unloaded`); A's entry survives only
            # because the stale lease still uses it.
            service.run("b")
            self.assertEqual(service.active_key, "b")
        worker.join(_TIMEOUT_S)
        self.assertFalse(worker.is_alive())
        self.assertEqual(failures, [])

        # The second request reloaded A under its stale lease ...
        self.assertEqual(service.loads, ["a", "b", "a"])
        self.assertEqual(service.active_key, "a")
        # ... and the manager agrees: exactly one model resident, nothing held.
        health = manager.health()
        self.assertEqual(health["resident_model_count"], 1)
        self.assertEqual(health["active_model_count"], 0)
        self.assertEqual(health["loading_model_count"], 0)

        # A is evictable: at cap 1 a third key reclaims the slot through A's callback.
        manager.set_max_loaded_models(1)
        self.assertEqual(service.active_key, "a")
        lease = manager.begin_model_use("c")
        self.addCleanup(lease.release)
        self.assertTrue(lease.needs_load)
        self.assertIsNone(service.active_key)


if __name__ == "__main__":
    unittest.main()
