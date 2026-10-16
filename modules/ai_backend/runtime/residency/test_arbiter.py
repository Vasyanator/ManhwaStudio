"""
File: modules/ai_backend/runtime/residency/test_arbiter.py

Purpose:
Contract tests of `ResidencyArbiter` with fake probes and a fake large-pool
policy next to the real small pool (`runtime/model_manager.py`).

Main responsibilities:
- reclaim order R4: A park idle large (only while the host fits) < B idle small
  (LRU) < C drop idle large;
- R1: in-use, loading and evicting holdings are never offered;
- the owner rule: two threads in IN_RUN on two owners, each holding the other's
  idle entry, both finish (Barrier + join timeout); the SDXL-takes-LaMa-lease
  shape;
- IN_RUN never waits; ADMISSION waits for an in-flight eviction, then succeeds;
- outstanding reservations subtract (two admissions cannot both take the last
  4 GiB);
- footprint: unknown default -> learned after a clean load, contaminated window
  ignored, release re-measure only raises;
- own torch slack is credited to the headroom, and the view keeps foreign /
  owned-by-pool / own slack apart;
- a raising reclaim is skipped, not propagated; `hint=None` reserves no bytes;
- `runtime/residency` imports no torch.

Notes:
Pure Python, no torch, no GPU. Every thread wait is bounded by `_TIMEOUT_S`.
"""

from __future__ import annotations

import subprocess
import sys
import threading
import unittest
from collections.abc import Callable, Sequence
from pathlib import Path

from modules.ai_backend.runtime.memory_budget import GIB, MIB, DeviceBudget, HostBudget
from modules.ai_backend.runtime.model_manager import SmallModelManager
from modules.ai_backend.runtime.residency.arbiter import ModelMemoryShortError, Reservation, ResidencyArbiter
from modules.ai_backend.runtime.residency.holdings import (
    Action,
    FootprintHint,
    Freed,
    HoldingsView,
    Need,
    Offer,
    OfferClass,
    Part,
    Pool,
    Requester,
    Runtime,
    Shortfall,
    Source,
    Tier,
    Urgency,
)

_TIMEOUT_S = 5.0
DEV = "cuda:0"


class FakeProbes:
    """Mutable host and one-device figures; the reserve is 0 unless set."""

    def __init__(self, *, host_available: int = 64 * GIB, device_total: int = 24 * GIB, device_free: int = 24 * GIB) -> None:
        self.lock = threading.Lock()
        self.host_available = host_available
        self.host_reserve = 0
        self.rss_anon = 0
        self.device_total = device_total
        self.device_free = device_free
        self.torch_allocated = 0
        self.torch_reserved = 0
        self.device_reserve = 0

    def host(self) -> HostBudget:
        with self.lock:
            return HostBudget(total=128 * GIB, available=self.host_available, cgroup_limit=None, cgroup_used=None, rss_anon=self.rss_anon, rss_shmem=0, reserve=self.host_reserve)

    def device(self, device: str) -> DeviceBudget:
        with self.lock:
            non_torch = max(0, self.device_total - self.device_free - self.torch_reserved)
            return DeviceBudget(device=device, total=self.device_total, free=self.device_free, torch_allocated=self.torch_allocated, torch_reserved=self.torch_reserved, non_torch=non_torch, reserve=self.device_reserve, cap=None)

    def free_device(self, amount: int) -> None:
        """A torch model of `amount` bytes was dropped (and its cache released)."""
        with self.lock:
            self.device_free += amount
            self.torch_allocated = max(0, self.torch_allocated - amount)
            self.torch_reserved = max(0, self.torch_reserved - amount)

    def use_device(self, amount: int) -> None:
        """A torch model of `amount` bytes was loaded."""
        with self.lock:
            self.device_free -= amount
            self.torch_allocated += amount
            self.torch_reserved += amount


class Clock:
    """Strictly increasing fake clock (LRU order is then the call order)."""

    def __init__(self) -> None:
        self._now = 0.0
        self._lock = threading.Lock()

    def __call__(self) -> float:
        with self._lock:
            self._now += 1.0
            return self._now


class FakeLargePolicy:
    """A stand-in large pool: per-component PARK_TO_HOST / DROP offers of idle holdings."""

    pool = Pool.LARGE

    def __init__(self, arbiter: ResidencyArbiter, probes: FakeProbes) -> None:
        self.arbiter = arbiter
        self.probes = probes
        self.executed: list[tuple[str, str, str]] = []
        arbiter.register_pool(self)

    def slot_capacity(self) -> int | None:
        return None

    def offers(self, view: HoldingsView, need: Need, requester: Requester | None) -> Sequence[Offer]:
        offers: list[Offer] = []
        for holding in view.of_pool(Pool.LARGE):
            if not holding.idle:
                continue
            for part in holding.parts:
                if part.tier is Tier.DEVICE:
                    offers.append(Offer(pool=Pool.LARGE, key=holding.key, part=part.name, action=Action.PARK_TO_HOST, device=DEV, frees_device=part.device_bytes, frees_host=-part.device_bytes, reload_cost_s=1.0, cls=OfferClass.A, last_used_at=holding.last_used_at))
                offers.append(Offer(pool=Pool.LARGE, key=holding.key, part=part.name, action=Action.DROP, device=DEV, frees_device=part.device_bytes, frees_host=part.host_bytes, reload_cost_s=30.0, cls=OfferClass.C, last_used_at=holding.last_used_at))
        return offers

    def execute(self, offer: Offer) -> Freed:
        self.executed.append((offer.key, offer.part or "", offer.action.value))
        parts = list(self.arbiter.holdings(Pool.LARGE)[offer.key].parts)
        target = next(part for part in parts if part.name == offer.part)
        parts.remove(target)
        if offer.action is Action.PARK_TO_HOST:
            parts.append(Part(name=target.name, device_bytes=0, host_bytes=target.device_bytes, tier=Tier.HOST, reload_cost_s=1.0))
        self.probes.free_device(target.device_bytes)
        if parts:
            self.arbiter.update_parts(Pool.LARGE, offer.key, parts, device=DEV)
        else:
            self.arbiter.mark_unloaded(Pool.LARGE, offer.key)
        return Freed(device_bytes=target.device_bytes, host_bytes=offer.frees_host, ok=True)


class Unloads:
    """Small-model unload callbacks that record the call and give the bytes back to the fake device."""

    def __init__(self, manager: SmallModelManager, probes: FakeProbes) -> None:
        self.manager = manager
        self.probes = probes
        self.calls: list[str] = []

    def callback(self, key: str, device_bytes: int) -> Callable[[], bool]:
        def unload() -> bool:
            self.calls.append(key)
            self.probes.free_device(device_bytes)
            self.manager.mark_unloaded(key)
            return True

        return unload


def _setup(**probe_kwargs: int) -> tuple[ResidencyArbiter, FakeProbes, SmallModelManager, FakeLargePolicy, Unloads]:
    probes = FakeProbes(**probe_kwargs)
    arbiter = ResidencyArbiter(probes, clock=Clock(), admission_wait_s=_TIMEOUT_S)
    manager = SmallModelManager(max_loaded_models=10, arbiter=arbiter)
    large = FakeLargePolicy(arbiter, probes)
    return arbiter, probes, manager, large, Unloads(manager, probes)


def _load_small(manager: SmallModelManager, unloads: Unloads, key: str, device_bytes: int, *, owner: str | None = None) -> None:
    """An idle resident small model of `device_bytes` on the fake device."""
    callback = unloads.callback(key, device_bytes)
    lease = manager.begin_model_use(key, callback, owner=owner, hint=FootprintHint(device=DEV, device_bytes=device_bytes, host_bytes=0))
    unloads.probes.use_device(device_bytes)
    lease.mark_loaded(callback)
    lease.release()


def _load_large(arbiter: ResidencyArbiter, probes: FakeProbes, key: str, device_bytes: int, *, owner: str = "flux") -> None:
    """An idle resident large model with one device component `dit`."""
    lease = arbiter.begin_use(Pool.LARGE, key, owner=owner)
    probes.use_device(device_bytes)
    lease.mark_loaded()
    arbiter.update_parts(Pool.LARGE, key, [Part(name="dit", device_bytes=device_bytes, host_bytes=0, tier=Tier.DEVICE, reload_cost_s=30.0)], device=DEV)
    lease.release()


def _measure_once(manager: SmallModelManager, probes: FakeProbes, key: str, device_bytes: int) -> None:
    """Load `key` once in a clean window that measures `device_bytes`, then drop it (its size is now LEARNED)."""
    lease = manager.begin_model_use(key, hint=FootprintHint(device=DEV, device_bytes=device_bytes, host_bytes=0))
    with probes.lock:
        probes.torch_reserved += device_bytes
    lease.mark_loaded(lambda: True)
    lease.release()
    with probes.lock:
        probes.torch_reserved -= device_bytes
    manager.mark_unloaded(key)
    assert manager.holdings() == {} or key not in manager.holdings()


def _requester(urgency: Urgency = Urgency.ADMISSION, *, owner: str = "req", pool: Pool = Pool.LARGE) -> Requester:
    return Requester(pool=pool, key="requester", owner=owner, urgency=urgency)


class ReclaimOrderTests(unittest.TestCase):
    def test_park_of_an_idle_large_component_comes_first_when_the_host_fits(self) -> None:
        arbiter, probes, manager, large, unloads = _setup()
        _load_large(arbiter, probes, "L", 4 * GIB)
        _load_small(manager, unloads, "s1", 2 * GIB)
        _load_small(manager, unloads, "s2", 2 * GIB)
        probes.device_free = 1 * GIB
        result = arbiter.reserve(_requester(), Need(device=DEV, device_bytes=3 * GIB))
        self.assertIsInstance(result, Reservation)
        self.assertEqual(large.executed, [("L", "dit", "park_to_host")])
        self.assertEqual(unloads.calls, [])

    def test_small_models_lru_when_a_park_would_not_fit_the_host(self) -> None:
        arbiter, probes, manager, large, unloads = _setup()
        _load_large(arbiter, probes, "L", 4 * GIB)
        _load_small(manager, unloads, "s1", 2 * GIB)
        _load_small(manager, unloads, "s2", 2 * GIB)
        _load_small(manager, unloads, "s3", 2 * GIB)
        probes.device_free = 1 * GIB
        probes.host_available = 1 * GIB  # a 4 GiB park cannot land in RAM
        result = arbiter.reserve(_requester(), Need(device=DEV, device_bytes=4 * GIB))
        self.assertIsInstance(result, Reservation)
        # Oldest first, and only as many as the deficit (3 GiB) needs.
        self.assertEqual(unloads.calls, ["s1", "s2"])
        self.assertEqual(large.executed, [])

    def test_an_idle_large_drop_is_the_last_resort(self) -> None:
        arbiter, probes, manager, large, unloads = _setup()
        _load_large(arbiter, probes, "L", 4 * GIB)
        _load_small(manager, unloads, "s1", 2 * GIB)
        probes.device_free = 1 * GIB
        probes.host_available = 1 * GIB
        result = arbiter.reserve(_requester(), Need(device=DEV, device_bytes=6 * GIB))
        self.assertIsInstance(result, Reservation)
        self.assertEqual(unloads.calls, ["s1"])
        self.assertEqual(large.executed, [("L", "dit", "drop")])

    def test_nothing_is_evicted_when_the_offers_cannot_cover_the_deficit(self) -> None:
        arbiter, probes, manager, large, unloads = _setup()
        _load_small(manager, unloads, "s1", 2 * GIB)
        probes.device_free = 1 * GIB
        result = arbiter.reserve(_requester(), Need(device=DEV, device_bytes=8 * GIB))
        self.assertIsInstance(result, Shortfall)
        assert isinstance(result, Shortfall)
        self.assertEqual(result.resource, "device")
        self.assertEqual(result.deficit, 7 * GIB)
        self.assertEqual(unloads.calls, [])


class R1Tests(unittest.TestCase):
    def test_in_use_and_loading_holdings_are_never_offered(self) -> None:
        arbiter, probes, manager, large, unloads = _setup()
        _load_small(manager, unloads, "used", 2 * GIB, owner="ocr")
        held = manager.begin_model_use("used", owner="ocr")
        self.addCleanup(held.release)
        loading = manager.begin_model_use("loading", owner="det", hint=FootprintHint(device=DEV, device_bytes=1 * GIB, host_bytes=0))
        self.addCleanup(loading.release)
        self.assertTrue(loading.needs_load)
        probes.device_free = 0
        result = arbiter.reserve(_requester(Urgency.IN_RUN), Need(device=DEV, device_bytes=1 * GIB))
        self.assertIsInstance(result, Shortfall)
        assert isinstance(result, Shortfall)
        self.assertEqual(result.blocked_by, ("det", "ocr"))
        self.assertEqual(unloads.calls, [])

    def test_an_evicting_holding_is_not_offered_and_in_run_does_not_wait(self) -> None:
        arbiter, probes, manager, large, unloads = _setup()
        gate = threading.Event()
        entered = threading.Event()

        def slow_unload() -> bool:
            entered.set()
            gate.wait(_TIMEOUT_S)
            probes.free_device(2 * GIB)
            manager.mark_unloaded("s1")
            return True

        lease = manager.begin_model_use("s1", slow_unload, hint=FootprintHint(device=DEV, device_bytes=2 * GIB, host_bytes=0))
        probes.use_device(2 * GIB)
        lease.mark_loaded(slow_unload)
        lease.release()
        probes.device_free = 0
        first: list[object] = []
        evictor = threading.Thread(target=lambda: first.append(arbiter.reserve(_requester(owner="a"), Need(device=DEV, device_bytes=2 * GIB))))
        evictor.start()
        self.assertTrue(entered.wait(_TIMEOUT_S))
        # s1 is being evicted for the first requester: an IN_RUN request gets no offer and no wait.
        second = arbiter.reserve(_requester(Urgency.IN_RUN, owner="b"), Need(device=DEV, device_bytes=1 * GIB))
        self.assertIsInstance(second, Shortfall)
        gate.set()
        evictor.join(_TIMEOUT_S)
        self.assertFalse(evictor.is_alive())
        self.assertIsInstance(first[0], Reservation)


class WaitTests(unittest.TestCase):
    def test_admission_waits_for_an_in_flight_eviction_then_succeeds(self) -> None:
        arbiter, probes, manager, large, unloads = _setup()
        gate = threading.Event()
        entered = threading.Event()

        def slow_unload() -> bool:
            entered.set()
            gate.wait(_TIMEOUT_S)
            probes.free_device(4 * GIB)
            manager.mark_unloaded("big")
            return True

        lease = manager.begin_model_use("big", slow_unload, hint=FootprintHint(device=DEV, device_bytes=4 * GIB, host_bytes=0))
        probes.use_device(4 * GIB)
        lease.mark_loaded(slow_unload)
        lease.release()
        probes.device_free = 0
        # Another thread's IN_RUN reclaim evicts `big` (it needs 1 GiB, frees 4 GiB).
        evictor_result: list[object] = []
        evictor = threading.Thread(target=lambda: evictor_result.append(arbiter.reserve(_requester(Urgency.IN_RUN, owner="a"), Need(device=DEV, device_bytes=1 * GIB))))
        evictor.start()
        self.assertTrue(entered.wait(_TIMEOUT_S))
        threading.Timer(0.2, gate.set).start()
        # Nothing is offerable now; the admission waits for that eviction and then fits.
        result = arbiter.reserve(_requester(owner="b"), Need(device=DEV, device_bytes=2 * GIB))
        self.assertIsInstance(result, Reservation)
        evictor.join(_TIMEOUT_S)
        self.assertFalse(evictor.is_alive())

    def test_admission_without_evictions_in_flight_does_not_wait(self) -> None:
        arbiter, probes, manager, large, unloads = _setup(device_free=0)
        result = arbiter.reserve(_requester(), Need(device=DEV, device_bytes=1 * GIB))
        self.assertIsInstance(result, Shortfall)


class ReservationTests(unittest.TestCase):
    def test_two_concurrent_admissions_cannot_both_take_the_last_4_gib(self) -> None:
        arbiter, probes, manager, large, unloads = _setup(device_free=4 * GIB)
        barrier = threading.Barrier(2)
        results: list[object] = []
        lock = threading.Lock()

        def admit(owner: str) -> None:
            barrier.wait(_TIMEOUT_S)
            outcome = arbiter.reserve(_requester(owner=owner), Need(device=DEV, device_bytes=4 * GIB))
            with lock:
                results.append(outcome)

        threads = [threading.Thread(target=admit, args=(name,)) for name in ("a", "b")]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(_TIMEOUT_S)
            self.assertFalse(thread.is_alive())
        self.assertEqual(sorted(type(outcome).__name__ for outcome in results), ["Reservation", "Shortfall"])

    def test_a_released_reservation_frees_its_bytes(self) -> None:
        arbiter, probes, manager, large, unloads = _setup(device_free=4 * GIB)
        first = arbiter.reserve(_requester(owner="a"), Need(device=DEV, device_bytes=4 * GIB))
        assert isinstance(first, Reservation)
        self.assertIsInstance(arbiter.reserve(_requester(owner="b"), Need(device=DEV, device_bytes=1 * GIB)), Shortfall)
        arbiter.update_reservation(first, Need(device=DEV, device_bytes=3 * GIB))
        self.assertIsInstance(arbiter.reserve(_requester(owner="b"), Need(device=DEV, device_bytes=1 * GIB)), Reservation)
        arbiter.release_reservation(first)
        self.assertEqual(arbiter.health()["reservations"], {"count": 1, "device_bytes": {DEV: 1 * GIB}, "host_bytes": 0})

    def test_a_small_admission_reservation_materializes_at_finish_load(self) -> None:
        arbiter, probes, manager, large, unloads = _setup(device_free=4 * GIB)
        lease = manager.begin_model_use("m", hint=FootprintHint(device=DEV, device_bytes=3 * GIB, host_bytes=0))
        self.assertEqual(arbiter.health()["reservations"]["count"], 1)
        lease.mark_loaded(lambda: True)
        lease.release()
        self.assertEqual(arbiter.health()["reservations"]["count"], 0)

    def test_a_small_admission_short_of_memory_raises_with_the_resource(self) -> None:
        arbiter, probes, manager, large, unloads = _setup(device_free=8 * GIB)
        _measure_once(manager, probes, "m", 3 * GIB)
        probes.device_free = 1 * GIB
        with self.assertRaises(ModelMemoryShortError) as caught:
            manager.begin_model_use("m", hint=FootprintHint(device=DEV, device_bytes=3 * GIB, host_bytes=0))
        self.assertEqual(caught.exception.shortfall.resource, "device")
        self.assertIn("видеопамяти", str(caught.exception))
        self.assertEqual(manager.health()["loading_model_count"], 0)
        self.assertEqual(manager.health()["active_model_count"], 0)

    def test_a_lease_without_hint_reserves_no_bytes(self) -> None:
        # Unknown device memory (all zeros) would refuse any byte need; hint=None needs none.
        arbiter, probes, manager, large, unloads = _setup(device_total=0, device_free=0, host_available=0)
        lease = manager.begin_model_use("m")
        self.assertTrue(lease.needs_load)
        lease.mark_loaded(lambda: True)
        lease.release()
        self.assertEqual(manager.health()["resident_model_count"], 1)


class OwnerRuleTests(unittest.TestCase):
    def test_two_in_run_owners_holding_each_others_idle_entry_finish(self) -> None:
        """Without the owner rule T1 would wait in B's unload for lock_b while T2 waits in A's for lock_a."""
        arbiter, probes, manager, large, unloads = _setup()
        locks = {"a": threading.Lock(), "b": threading.Lock()}

        def unload_under(owner: str, key: str) -> Callable[[], bool]:
            def unload() -> bool:
                with locks[owner]:
                    manager.mark_unloaded(key)
                    return True

            return unload

        for owner in ("a", "b"):
            idle_key = f"{owner}:idle"
            lease = manager.begin_model_use(idle_key, unload_under(owner, idle_key), owner=owner, hint=FootprintHint(device=DEV, device_bytes=2 * GIB, host_bytes=0))
            lease.mark_loaded(unload_under(owner, idle_key))
            lease.release()
        probes.device_free = 0
        barrier = threading.Barrier(2)
        results: dict[str, object] = {}

        def run(owner: str) -> None:
            lease = manager.begin_model_use(f"{owner}:busy", owner=owner)
            try:
                with locks[owner]:
                    lease.mark_loaded(lambda: True)
                    barrier.wait(_TIMEOUT_S)
                    # In its run, under its own service lock, each owner needs memory the
                    # other owner's idle model holds.
                    results[owner] = arbiter.reserve(Requester(pool=Pool.SMALL, key=f"{owner}:busy", owner=owner, urgency=Urgency.IN_RUN, holds_lease=True), Need(device=DEV, device_bytes=1 * GIB))
            finally:
                lease.release()

        threads = [threading.Thread(target=run, args=(owner,), daemon=True) for owner in ("a", "b")]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(_TIMEOUT_S)
            self.assertFalse(thread.is_alive(), "deadlock: an IN_RUN reclaim waited on a busy owner's lock")
        self.assertIsInstance(results["a"], Shortfall)
        self.assertIsInstance(results["b"], Shortfall)

    def test_an_idle_model_of_a_non_busy_owner_is_evicted_in_run(self) -> None:
        arbiter, probes, manager, large, unloads = _setup()
        _load_small(manager, unloads, "other", 2 * GIB, owner="ocr")
        probes.device_free = 0
        result = arbiter.reserve(_requester(Urgency.IN_RUN, owner="flux"), Need(device=DEV, device_bytes=1 * GIB))
        self.assertIsInstance(result, Reservation)
        self.assertEqual(unloads.calls, ["other"])

    def test_sdxl_holding_its_lease_never_loses_its_idle_entries_to_the_nested_lama_admission(self) -> None:
        arbiter, probes, manager, large, unloads = _setup()
        _load_small(manager, unloads, "sdxl:other_mode", 3 * GIB, owner="sdxl")
        _load_small(manager, unloads, "lama:old_ckpt", 1 * GIB, owner="lama")
        sdxl_lease = manager.begin_model_use("sdxl:run", owner="sdxl")
        sdxl_lease.mark_loaded(lambda: True)
        try:
            probes.device_free = 0
            # Inside its run (lock held in real code) SDXL takes the LaMa lease.
            lama = manager.begin_model_use("lama:ckpt", owner="lama", hint=FootprintHint(device=DEV, device_bytes=1 * GIB, host_bytes=0))
            lama.mark_loaded(lambda: True)
            lama.release()
            # The LaMa admission took LaMa's own idle model; SDXL's idle entry survived.
            self.assertEqual(unloads.calls, ["lama:old_ckpt"])
            # The nested admission never reaches SDXL's idle entry: blocked by the busy owner.
            blocked = arbiter.reserve(Requester(pool=Pool.SMALL, key="lama:big", owner="lama", urgency=Urgency.ADMISSION), Need(device=DEV, device_bytes=2 * GIB))
            assert isinstance(blocked, Shortfall)
            self.assertIn("sdxl", blocked.blocked_by)
            self.assertEqual(unloads.calls, ["lama:old_ckpt"])
        finally:
            sdxl_lease.release()


class FailureTests(unittest.TestCase):
    def test_a_raising_reclaim_is_skipped_and_the_next_candidate_used(self) -> None:
        arbiter, probes, manager, large, unloads = _setup()

        def boom() -> bool:
            raise OSError("unload failed")

        lease = manager.begin_model_use("bad", boom, hint=FootprintHint(device=DEV, device_bytes=2 * GIB, host_bytes=0))
        probes.use_device(2 * GIB)
        lease.mark_loaded(boom)
        lease.release()
        _load_small(manager, unloads, "good", 2 * GIB)
        probes.device_free = 0
        with self.assertLogs("modules.ai_backend.runtime.residency.arbiter", level="ERROR"):
            result = arbiter.reserve(_requester(), Need(device=DEV, device_bytes=1 * GIB))
        self.assertIsInstance(result, Reservation)
        self.assertEqual(unloads.calls, ["good"])
        self.assertTrue(manager.holdings()["bad"].resident)


class FootprintMeasurementTests(unittest.TestCase):
    def test_unknown_size_defaults_then_is_learned_after_a_clean_load(self) -> None:
        arbiter, probes, manager, large, unloads = _setup()
        hint = FootprintHint(device=DEV, device_bytes=None, host_bytes=None)
        lease = manager.begin_model_use("m", hint=hint)
        self.assertEqual(manager.holdings()["m"].footprint.source, Source.DEFAULT)
        self.assertEqual(manager.holdings()["m"].footprint.device_bytes, 2 * GIB)
        with probes.lock:
            probes.torch_reserved += 3 * GIB
            probes.device_free -= 3 * GIB
            probes.rss_anon += 300 * MIB
        lease.mark_loaded(lambda: True)
        lease.release()
        recorded = manager.holdings()["m"].footprint
        self.assertEqual((recorded.source, recorded.device_bytes, recorded.host_bytes), (Source.MEASURED, 3 * GIB, 300 * MIB))
        manager.mark_unloaded("m")
        again = manager.begin_model_use("m", hint=hint)
        self.addCleanup(again.release)
        estimate = manager.holdings()["m"].footprint
        self.assertEqual((estimate.source, estimate.device_bytes), (Source.LEARNED, 3 * GIB))

    def test_a_contaminated_window_keeps_the_estimate(self) -> None:
        arbiter, probes, manager, large, unloads = _setup()
        lease = manager.begin_model_use("m", hint=FootprintHint(device=DEV, device_bytes=1 * GIB, host_bytes=0))
        other = manager.begin_model_use("other")  # another load overlaps this one
        with probes.lock:
            probes.torch_reserved += 5 * GIB
        lease.mark_loaded(lambda: True)
        lease.release()
        other.mark_load_failed()
        other.release()
        recorded = manager.holdings()["m"].footprint
        self.assertEqual((recorded.source, recorded.device_bytes), (Source.DECLARED, 1 * GIB))

    def test_a_clean_release_window_only_raises_the_footprint(self) -> None:
        arbiter, probes, manager, large, unloads = _setup()
        hint = FootprintHint(device=DEV, device_bytes=1 * GIB, host_bytes=0, runtime=Runtime.ONNX)
        lease = manager.begin_model_use("onnx", hint=hint)
        lease.mark_loaded(lambda: True)
        lease.release()
        self.assertEqual(manager.holdings()["onnx"].footprint.device_bytes, 1 * GIB)
        # First inference grows the arena by 512 MiB ...
        lease = manager.begin_model_use("onnx", hint=hint)
        lease.mark_loaded(lambda: True)
        probes.use_device(512 * MIB)
        lease.release()
        self.assertEqual(manager.holdings()["onnx"].footprint.device_bytes, 1 * GIB + 512 * MIB)
        # ... a later lease that ends with MORE free memory never lowers it.
        lease = manager.begin_model_use("onnx", hint=hint)
        probes.free_device(1 * GIB)
        lease.release()
        self.assertEqual(manager.holdings()["onnx"].footprint.device_bytes, 1 * GIB + 512 * MIB)


class SlackAccountingTests(unittest.TestCase):
    def test_own_torch_slack_is_usable_and_the_view_keeps_the_three_parts_apart(self) -> None:
        arbiter, probes, manager, large, unloads = _setup()
        _load_small(manager, unloads, "ocr_model", 4 * GIB, owner="ocr")
        _load_small(manager, unloads, "lama_model", 2 * GIB, owner="lama")
        reserve = 512 * MIB
        other_process = 4 * GIB
        allocated = 6 * GIB  # the two models above
        slack = 2 * GIB
        with probes.lock:
            probes.device_reserve = reserve
            probes.torch_allocated = allocated
            probes.torch_reserved = allocated + slack
            probes.device_free = probes.device_total - other_process - allocated - slack
        account = arbiter.view(DEV).devices[DEV]
        self.assertEqual(account.headroom, 24 * GIB - other_process - allocated - reserve)
        self.assertEqual(account.headroom_without_slack, 24 * GIB - other_process - allocated - slack - reserve)
        self.assertEqual(account.foreign_bytes, other_process)
        self.assertEqual(account.own_slack_bytes, slack)
        self.assertEqual(dict(account.held_by_pool[Pool.SMALL]), {"ocr": 4 * GIB, "lama": 2 * GIB})
        # A torch need may use the slack; an ONNX need may not.
        fits = 24 * GIB - other_process - allocated - reserve
        self.assertIsInstance(arbiter.reserve(_requester(owner="t"), Need(device=DEV, device_bytes=fits)), Reservation)

    def test_an_onnx_need_does_not_get_the_slack_credit(self) -> None:
        arbiter, probes, manager, large, unloads = _setup()
        with probes.lock:
            probes.torch_allocated = 0
            probes.torch_reserved = 4 * GIB
            probes.device_free = 20 * GIB
        result = arbiter.reserve(_requester(owner="o"), Need(device=DEV, device_bytes=22 * GIB, torch_slack_usable=False))
        self.assertIsInstance(result, Shortfall)
        self.assertIsInstance(arbiter.reserve(_requester(owner="t"), Need(device=DEV, device_bytes=22 * GIB)), Reservation)

    def test_reclaimable_bytes_per_pool_are_visible(self) -> None:
        arbiter, probes, manager, large, unloads = _setup()
        _load_large(arbiter, probes, "L", 4 * GIB)
        _load_small(manager, unloads, "s1", 2 * GIB)
        busy = manager.begin_model_use("s2", owner="busy", hint=FootprintHint(device=DEV, device_bytes=1 * GIB, host_bytes=0))
        self.addCleanup(busy.release)
        view = arbiter.view(DEV)
        self.assertEqual(view.reclaimable(Pool.SMALL, device=DEV).device_bytes, 2 * GIB)
        self.assertEqual(view.reclaimable(Pool.LARGE, device=DEV).device_bytes, 4 * GIB)


class LenientAdmissionTests(unittest.TestCase):
    def test_an_unreadable_device_is_not_byte_checked_for_a_small_load(self) -> None:
        arbiter, probes, manager, large, unloads = _setup(device_total=0, device_free=0)
        with self.assertLogs("modules.ai_backend.runtime.residency.arbiter", level="WARNING") as logged:
            _measure_once(manager, probes, "m", 3 * GIB)  # its size is now LEARNED (strict) ...
            lease = manager.begin_model_use("m", hint=FootprintHint(device=DEV, device_bytes=3 * GIB, host_bytes=0))
            self.assertEqual(manager.holdings()["m"].footprint.source, Source.LEARNED)  # ... and still admitted
            lease.mark_loaded(lambda: True)
            lease.release()
            second = manager.begin_model_use("n", hint=FootprintHint(device=DEV, device_bytes=GIB, host_bytes=0))
            second.mark_loaded(lambda: True)
            second.release()
        self.assertEqual(sum("cannot be read" in line for line in logged.output), 1)  # once per device
        # The large pool keeps "unknown = refuse".
        self.assertIsInstance(arbiter.reserve(_requester(), Need(device=DEV, device_bytes=GIB)), Shortfall)

    def test_an_unmeasured_hint_makes_room_but_never_refuses(self) -> None:
        arbiter, probes, manager, large, unloads = _setup()
        _load_small(manager, unloads, "idle", 2 * GIB)
        probes.device_free = 1 * GIB
        with self.assertLogs("modules.ai_backend.runtime.residency.arbiter", level="INFO") as logged:
            lease = manager.begin_model_use("big", hint=FootprintHint(device=DEV, device_bytes=8 * GIB, host_bytes=0))
        self.assertTrue(lease.needs_load)
        self.assertEqual(unloads.calls, ["idle"])  # room was made
        self.assertTrue(any("unmeasured estimate" in line for line in logged.output))
        lease.mark_load_failed()
        lease.release()

    def test_a_measured_size_that_does_not_fit_refuses(self) -> None:
        arbiter, probes, manager, large, unloads = _setup()
        _measure_once(manager, probes, "big", 8 * GIB)
        probes.device_free = 1 * GIB
        with self.assertRaises(ModelMemoryShortError):
            manager.begin_model_use("big", hint=FootprintHint(device=DEV, device_bytes=8 * GIB, host_bytes=0))

    def test_a_partial_torch_module_does_not_break_the_reserve_default(self) -> None:
        import types
        from unittest.mock import patch

        from modules.ai_backend.runtime.residency import arbiter as arbiter_module

        fake_torch = types.ModuleType("torch")
        with patch.dict(sys.modules, {"torch": fake_torch}), patch.object(arbiter_module, "is_torch_available", lambda: True):
            self.assertFalse(arbiter_module._is_rocm_build())


class ImportTests(unittest.TestCase):
    def test_residency_imports_no_torch(self) -> None:
        repo_root = Path(__file__).resolve().parents[4]
        code = (
            "import sys\n"
            "import modules.ai_backend.runtime.residency.arbiter\n"
            "import modules.ai_backend.runtime.residency.footprint\n"
            "import modules.ai_backend.runtime.model_manager\n"
            "assert 'torch' not in sys.modules, 'torch imported'\n"
        )
        completed = subprocess.run([sys.executable, "-c", code], cwd=repo_root, capture_output=True, text=True, timeout=60, check=False)
        self.assertEqual(completed.returncode, 0, completed.stderr)


if __name__ == "__main__":
    unittest.main()
