"""
File: modules/ai_backend/runtime/test_model_manager_pool.py

Purpose:
The small pool's policy over the residency arbiter (`model_manager.py`): the
count cap `General.ai_max_loaded_models` is still enforced as slots, LRU first,
a lowered cap evicts idle models live, a model without an unload callback or in
use is never evicted, and the cap refusal keeps today's message.

Notes:
Pure Python, no torch; the arbiter gets fake probes that are never consulted
for these slot-only (hint-less) leases.
"""

from __future__ import annotations

import unittest
from typing import Callable

from modules.ai_backend.runtime.memory_budget import DeviceBudget, HostBudget
from modules.ai_backend.runtime.model_manager import SmallModelManager
from modules.ai_backend.runtime.residency.arbiter import SLOT_SHORTFALL_MESSAGE, ModelMemoryShortError, ResidencyArbiter


class _NoProbes:
    """Fails the test if a slot-only lease ever probes memory."""

    def host(self) -> HostBudget:
        raise AssertionError("a hint-less lease must not probe the host")

    def device(self, device: str) -> DeviceBudget:
        raise AssertionError("a hint-less lease must not probe a device")


class _Unloads:
    def __init__(self, manager: SmallModelManager) -> None:
        self.manager = manager
        self.calls: list[str] = []

    def callback(self, key: str) -> Callable[[], bool]:
        def unload() -> bool:
            self.calls.append(key)
            self.manager.mark_unloaded(key)
            return True

        return unload


def _manager(cap: int) -> tuple[SmallModelManager, _Unloads]:
    arbiter = ResidencyArbiter(_NoProbes())
    manager = SmallModelManager(max_loaded_models=cap, arbiter=arbiter)
    return manager, _Unloads(manager)


def _load(manager: SmallModelManager, unloads: _Unloads, key: str) -> None:
    lease = manager.begin_model_use(key, unloads.callback(key))
    lease.mark_loaded(unloads.callback(key))
    lease.release()


class CountCapTests(unittest.TestCase):
    def test_a_new_load_evicts_the_least_recently_used_idle_model(self) -> None:
        manager, unloads = _manager(2)
        _load(manager, unloads, "a")
        _load(manager, unloads, "b")
        _load(manager, unloads, "a")  # refresh: b is now the LRU
        _load(manager, unloads, "c")
        self.assertEqual(unloads.calls, ["b"])
        self.assertEqual(set(manager.holdings()), {"a", "c"})

    def test_lowering_the_cap_evicts_live(self) -> None:
        manager, unloads = _manager(3)
        for key in ("a", "b", "c"):
            _load(manager, unloads, key)
        self.assertEqual(manager.set_max_loaded_models(1), 1)
        self.assertEqual(unloads.calls, ["a", "b"])
        self.assertEqual(manager.health()["resident_model_count"], 1)
        self.assertEqual(manager.get_max_loaded_models(), 1)

    def test_in_use_and_callback_less_models_are_kept_and_the_cap_refusal_is_unchanged(self) -> None:
        manager, unloads = _manager(2)
        held = manager.begin_model_use("held", unloads.callback("held"))
        held.mark_loaded(unloads.callback("held"))
        bare = manager.begin_model_use("bare")
        bare.mark_loaded()
        bare.release()
        with self.assertRaises(ModelMemoryShortError) as caught:
            manager.begin_model_use("new")
        self.assertIsInstance(caught.exception, RuntimeError)
        self.assertEqual(str(caught.exception), SLOT_SHORTFALL_MESSAGE)
        self.assertEqual(unloads.calls, [])
        held.release()
        self.assertEqual(manager.health(), {"max_loaded_models": 2, "resident_model_count": 2, "active_model_count": 0, "loading_model_count": 0})

    def test_a_failed_unload_falls_through_to_the_next_candidate(self) -> None:
        manager, unloads = _manager(2)
        lease = manager.begin_model_use("stuck", lambda: False)
        lease.mark_loaded(lambda: False)
        lease.release()
        _load(manager, unloads, "b")
        _load(manager, unloads, "c")
        self.assertEqual(unloads.calls, ["b"])
        self.assertTrue(manager.holdings()["stuck"].resident)

    def test_the_cap_is_clamped(self) -> None:
        manager, _ = _manager(3)
        self.assertEqual(manager.set_max_loaded_models(99), 10)
        self.assertEqual(manager.set_max_loaded_models("x"), 3)
        self.assertEqual(SmallModelManager(max_loaded_models=0, arbiter=ResidencyArbiter(_NoProbes())).get_max_loaded_models(), 1)


if __name__ == "__main__":
    unittest.main()
