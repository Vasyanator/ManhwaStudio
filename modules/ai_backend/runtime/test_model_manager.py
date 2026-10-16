"""
File: modules/ai_backend/runtime/test_model_manager.py

Purpose:
Unit tests of `SmallModelManager` / `ModelUsageLease` that need no service:
the report calls of a lease.

Main responsibilities:
- `mark_loaded` registers the key on EVERY lease, also one that found the key
  resident (`needs_load=False`), and is idempotent;
- `mark_load_failed` is a no-op after `mark_loaded` and for a lease that did
  not reserve the load;
- `mark_unloaded` of a key another lease still uses keeps the entry (not
  resident), so the in-use count survives a concurrent unload;

Notes:
Pure Python, no torch: unload callbacks are plain recorders.
"""

from __future__ import annotations

import unittest
from typing import Callable

from modules.ai_backend.runtime.model_manager import SmallModelManager


class _Unloads:
    """Records which keys the manager asked to unload; each call succeeds."""

    def __init__(self) -> None:
        self.calls: list[str] = []

    def callback(self, key: str) -> Callable[[], bool]:
        """An unload callback for `key` that records the call and reports success."""

        def unload() -> bool:
            self.calls.append(key)
            return True

        return unload


def _load(manager: SmallModelManager, key: str, unloads: _Unloads) -> None:
    """Take a lease on `key`, register it resident and release it (an idle resident model)."""
    lease = manager.begin_model_use(key, unloads.callback(key))
    lease.mark_loaded(unloads.callback(key))
    lease.release()


class LeaseReportTests(unittest.TestCase):
    def test_mark_loaded_registers_under_a_lease_that_found_the_key_resident(self) -> None:
        manager = SmallModelManager(max_loaded_models=2)
        unloads = _Unloads()
        _load(manager, "a", unloads)
        lease = manager.begin_model_use("a", unloads.callback("a"))
        self.assertFalse(lease.needs_load)
        # Another request drops the model while this lease is held ...
        manager.mark_unloaded("a")
        self.assertEqual(manager.health()["resident_model_count"], 0)
        # ... and this request's service reloads it under the stale lease.
        lease.mark_loaded(unloads.callback("a"))
        lease.release()
        self.assertEqual(manager.health()["resident_model_count"], 1)

    def test_mark_loaded_is_idempotent(self) -> None:
        manager = SmallModelManager(max_loaded_models=2)
        unloads = _Unloads()
        lease = manager.begin_model_use("a", unloads.callback("a"))
        lease.mark_loaded(unloads.callback("a"))
        lease.mark_loaded(unloads.callback("a"))
        lease.release()
        health = manager.health()
        self.assertEqual(health["resident_model_count"], 1)
        self.assertEqual(health["loading_model_count"], 0)
        self.assertEqual(health["active_model_count"], 0)

    def test_mark_load_failed_after_mark_loaded_is_a_no_op(self) -> None:
        manager = SmallModelManager(max_loaded_models=2)
        unloads = _Unloads()
        lease = manager.begin_model_use("a", unloads.callback("a"))
        lease.mark_loaded(unloads.callback("a"))
        lease.mark_load_failed()
        lease.release()
        self.assertEqual(manager.health()["resident_model_count"], 1)

    def test_mark_load_failed_without_a_reserved_load_is_a_no_op(self) -> None:
        manager = SmallModelManager(max_loaded_models=2)
        unloads = _Unloads()
        _load(manager, "a", unloads)
        lease = manager.begin_model_use("a", unloads.callback("a"))
        self.assertFalse(lease.needs_load)
        lease.mark_load_failed()
        lease.release()
        self.assertEqual(manager.health()["resident_model_count"], 1)

    def test_a_failed_load_leaves_nothing_behind(self) -> None:
        manager = SmallModelManager(max_loaded_models=2)
        lease = manager.begin_model_use("a")
        self.assertTrue(lease.needs_load)
        lease.mark_load_failed()
        lease.release()
        self.assertEqual(
            manager.health(),
            {
                "max_loaded_models": 2,
                "resident_model_count": 0,
                "active_model_count": 0,
                "loading_model_count": 0,
            },
        )
        # The slot is free again: the next lease reserves the load.
        self.assertTrue(manager.begin_model_use("a").needs_load)

    def test_mark_unloaded_keeps_an_entry_another_lease_uses(self) -> None:
        manager = SmallModelManager(max_loaded_models=2)
        unloads = _Unloads()
        _load(manager, "a", unloads)
        lease = manager.begin_model_use("a", unloads.callback("a"))
        manager.mark_unloaded("a")
        health = manager.health()
        self.assertEqual(health["resident_model_count"], 0)
        self.assertEqual(health["active_model_count"], 1)
        lease.release()
        self.assertEqual(manager.health()["active_model_count"], 0)

    def test_an_empty_key_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            SmallModelManager().begin_model_use("  ")


if __name__ == "__main__":
    unittest.main()
