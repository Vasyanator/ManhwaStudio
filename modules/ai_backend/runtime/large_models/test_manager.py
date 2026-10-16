"""
File: modules/ai_backend/runtime/large_models/test_manager.py

Purpose:
Contract tests of `manager.py` with the REAL mechanics on CPU: a toy large
model of three real-torch components (`enc` 2 blocks, `dit` 6 blocks, `ae`
monolithic) written to real safetensors files, the real `ResidencyArbiter`
with the real small pool (`SmallModelManager`) beside it, fake budget probes
whose device usage is the fake transports' byte pools, and a fake transport
that keeps an exact byte ledger.

Covered contracts: a run matches the fully resident reference (also juggled);
a memory failure at each safe point (load, transition, phase) takes the
expected rung (reclaim, tighter device room, fallback, next VRAM step; a host
failure walks only the RAM axis); the retry cap; a host shortfall
mid-transition drops the remaining units, never allocates past the guard;
`finish` only shrinks (nothing loaded or moved up after an early error) and
`settle` loads; idle small models are evicted before any degrade; idle large
components are parked (host fits) or dropped (host short) before a small
admission is refused; an in-use handle is never offered; the run reservation
protects a running phase; the between-runs state per step pair; learned
floors per axis persisted and reset; Q-P1 («Авто» at most one VRAM step above
the free fit); admission refusal; the sysmem-spill degrade; the live host guard
before DISK staging (a budget that shrinks after admission); a `cpu` run device
planned, reserved and reported on the host side; the per-handle device
ceiling (`device_cap_bytes`) in profiles, admissions and ladder re-plans;
`placements` from
the ledger; health / status / profiles shapes; the store (old profile floors
and keep policy ignored); the Linux OOM backstop.

Notes:
Skips without torch. CPU only; files go to temporary directories.
"""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from typing import Any

try:
    import torch
except ImportError:  # pragma: no cover - the backend may run without torch
    torch = None  # type: ignore[assignment]

from ..memory_budget import DeviceBudget, HostBudget, SpillVerdict
from .degrade import FailureKind, FloorEntry, FloorKey, RungKind
from .manager import JsonLimitsStore, LargeModelRefused, LargeModelRunFailed, MemoryLimitsStore, apply_oom_score_backstop, limits_store_path
from .planner import PlanRefusal, Planned
from .spec import ComponentSpec, ExecDevice, Fallback, FormatInfo, LargeModelSpec, PhaseSpec, PhaseWork, RequestShape, Role
from .tiers import AUTO_STEPS, RAM_ORDER, Limits, RamStep, Steps, Tier, VramStep

ACT = 1024
SHAPE = RequestShape(width=64, height=64)
HUGE = 1 << 40
MAX_KEEP = Steps(VramStep.MAX, RamStep.KEEP)
PHASE_KEEP = Steps(VramStep.PHASE, RamStep.KEEP)
PHASE_RUN = Steps(VramStep.PHASE, RamStep.RUN)
HALF_KEEP = Steps(VramStep.HALF, RamStep.KEEP)
HALF_RUN = Steps(VramStep.HALF, RamStep.RUN)
MIN_KEEP = Steps(VramStep.MIN, RamStep.KEEP)


def _entry(phase: str, exec_kind: str, *, family: str = "toy") -> dict[str, Any]:
    return {"family": family, "phase": phase, "variant": "*", "format": "*", "compute_path": "*", "exec": exec_kind, "measured": True, "fixed_bytes": ACT, "bytes_per_token": 0, "bytes_per_pixel": 0, "reserved_factor": 1.0, "seconds_fixed": 0.01, "seconds_per_ktoken_pass": 0.0, "seconds_per_ktoken2_pass": 0.0, "seconds_per_mpixel": 0.0}


def _tables() -> Any:
    from .cost_tables import parse_tables

    rate = 1 << 30
    data = {
        "device_class": "test",
        "calibrated": True,
        "source": "test",
        "device": {"runtime_overhead_bytes": 0, "margin_fraction": 0.0, "margin_cap_bytes": 0, "park_bytes_per_s": rate, "restore_bytes_per_s": rate, "block_h2d_bytes_per_s": rate, "disk_cold_bytes_per_s": rate, "disk_warm_bytes_per_s": rate, "prefetch_depth": 1},
        "phases": [_entry("encode", "device"), _entry("encode", "cpu"), _entry("denoise", "device"), _entry("decode", "device"), _entry("*", "device", family="*"), _entry("*", "cpu", family="*")],
    }
    return parse_tables(data, source="test")


class _Probes:
    """Budget probes over the rig: device usage = the small models' torch bytes + every fake transport's device pool.

    Small models live in the (fake) torch caching allocator: their bytes move
    `torch_reserved` (and `torch_allocated`, so there is no slack), which is what
    the arbiter measures a clean small load with. The large toy's bytes are
    reported as non-torch (large holdings are never measured).
    """

    def __init__(self, rig: _Rig, *, device_total: int, host_available: int) -> None:
        self.rig = rig
        self.device_total = device_total
        self.host_available = host_available
        self.small_bytes = 0

    def host(self) -> HostBudget:
        return HostBudget(total=HUGE, available=self.host_available, cgroup_limit=None, cgroup_used=None, rss_anon=1 << 20, rss_shmem=0, reserve=0)

    def device(self, device: str) -> DeviceBudget:
        used = min(self.device_total, self.small_bytes + sum(transport.device_bytes() for transport in self.rig.transports))
        return DeviceBudget(device=device, total=self.device_total, free=self.device_total - used, torch_allocated=self.small_bytes, torch_reserved=self.small_bytes, non_torch=max(0, used - self.small_bytes), reserve=0, cap=None)


class _Rig:
    """A toy large model, the shared arbiter, the small pool and the manager under test."""

    def __init__(self, test: unittest.TestCase, *, device_total: int = HUGE, host_available: int = HUGE, fallbacks: tuple[Fallback, ...] = (), store: Any = None, transport_cls: Any = None, spill_detector: Any = None, max_retries: int = 3, small_slots: int = 10, encoder_cpu: bool = True) -> None:
        from . import _mechanics_test_fixtures as fx
        from ..model_manager import SmallModelManager
        from ..residency.arbiter import ResidencyArbiter
        from .manager import LargeModelManager

        self.fx = fx
        self._dir = tempfile.TemporaryDirectory()
        test.addCleanup(self._dir.cleanup)
        root = Path(self._dir.name)
        self.refs = {"enc": fx.reference_toy(2, seed=2), "dit": fx.reference_toy(6, seed=1), "ae": fx.reference_toy(0, seed=3)}
        self.paths = {name: fx.write_toy_checkpoint(root / f"{name}.safetensors", net) for name, net in self.refs.items()}
        self.meta_failures: dict[str, list[BaseException]] = {}
        self.encoder_cpu = encoder_cpu
        self.spec = self._spec(fallbacks)
        self.transports: list[Any] = []
        self.transport_cls = transport_cls or fx.FakeTransport
        self.probes = _Probes(self, device_total=device_total, host_available=host_available)
        self.arbiter = ResidencyArbiter(self.probes, admission_wait_s=0.0)
        self.small = SmallModelManager(small_slots, arbiter=self.arbiter)
        self.store = store if store is not None else MemoryLimitsStore()
        self.manager = LargeModelManager(self.arbiter, limits_store=self.store, tables=_tables(), probes=self.probes, transport_factory=self._transport, device_class=lambda device: "test_class", spill_detector=spill_detector, max_retries=max_retries)
        self.manager.register(self.spec, model="toy")
        self.x = torch.randn(4, 8, generator=torch.Generator().manual_seed(5))
        with torch.no_grad():
            self.expected = {name: net(self.x) for name, net in self.refs.items()}
        self.calls: dict[str, int] = {}

    def _spec(self, fallbacks: tuple[Fallback, ...]) -> LargeModelSpec:
        fx = self.fx
        rig = self

        def build(name: str, blocks: int) -> Any:
            def build_meta(options: Any, weight_format: str) -> Any:
                pending = rig.meta_failures.get(name)
                if pending:
                    raise pending.pop(0)
                return fx.build_toy_meta(blocks)

            return build_meta

        def component(name: str, role: Role, blocks: int, allowed: frozenset[ExecDevice]) -> ComponentSpec:
            path = self.paths[name]
            return ComponentSpec(name, role, lambda options, weight_format: [path], fx.toy_block_of, exec_devices_allowed=allowed, build_meta=build(name, blocks))

        def work(passes: int) -> Any:
            return lambda shape, options: PhaseWork(tokens=0, passes=passes)

        device_only = frozenset({ExecDevice.RUN_DEVICE})
        both = frozenset({ExecDevice.RUN_DEVICE, ExecDevice.CPU}) if self.encoder_cpu else device_only
        return LargeModelSpec(
            family="toy",
            components=(component("enc", Role.ENCODER, 2, both), component("dit", Role.DIT, 6, device_only), component("ae", Role.AE, 0, device_only)),
            phases=(PhaseSpec("encode", frozenset({"enc"}), "enc", work(1)), PhaseSpec("denoise", frozenset({"dit"}), "dit", work(2)), PhaseSpec("decode", frozenset({"ae"}), "ae", work(1))),
            formats=lambda options: {"f32": FormatInfo(True, None, 0, "native", 1)},
            fallbacks=fallbacks,
        )

    def _transport(self, device: str) -> Any:
        transport = self.transport_cls()
        self.transports.append(transport)
        return transport

    # ------------------------------------------------------------- driving
    def acquire(self, steps: Steps = AUTO_STEPS, **kwargs: Any) -> Any:
        return self.manager.acquire("toy", {}, SHAPE, device="cuda:0", format="f32", steps=steps, **kwargs)

    def phase_fn(self, phase: str, component: str, *, before: Any = None) -> Any:
        def fn(ctx: Any) -> Any:
            self.calls[phase] = self.calls.get(phase, 0) + 1
            if before is not None:
                before(ctx)
            with torch.no_grad():
                return ctx.module(component)(self.x)

        return fn

    def run(self, session: Any, **befores: Any) -> dict[str, Any]:
        outputs = {}
        for phase, component in (("encode", "enc"), ("denoise", "dit"), ("decode", "ae")):
            outputs[component] = session.run_phase(phase, self.phase_fn(phase, component, before=befores.get(phase)))
        return outputs

    def handle(self) -> Any:
        handles = list(self.manager._handles.values())
        assert len(handles) == 1
        return handles[0]

    def tiers(self, component: str) -> set[Tier]:
        handle = self.handle()
        if component not in handle.components:
            return {Tier.DROPPED}
        return {state.tier for state in handle.ledger.units(component)}

    def peaks(self) -> dict[Steps, int]:
        """Each step pair's forecast device peak on an ample card (non-juggling steps do not depend on it); unsupported pairs are left out."""
        saved = self.probes.device_total
        self.probes.device_total = HUGE
        found: dict[Steps, int] = {}
        try:
            for ram in RAM_ORDER:
                report = self.manager.profiles("toy", {}, SHAPE, device="cuda:0", format="f32", steps=Steps(VramStep.MAX, ram))
                for vram, result in report.vram_rows.items():
                    if result.forecast is not None:
                        found[Steps(vram, ram)] = result.forecast.device_peak
        finally:
            self.probes.device_total = saved
        return found

    def load_small(self, key: str, nbytes: int) -> list[str]:
        """A resident idle small model of `nbytes` device bytes; returns the list its unload appends to."""
        from ..residency.holdings import FootprintHint

        unloaded: list[str] = []
        lease = self.small.begin_model_use(key, None, owner=key, hint=FootprintHint(device="cuda:0", device_bytes=nbytes, host_bytes=0))
        self.probes.small_bytes += nbytes

        def unload() -> bool:
            self.probes.small_bytes -= nbytes
            unloaded.append(key)
            return True

        lease.mark_loaded(unload)
        lease.release()
        return unloaded

    def measure_once(self, key: str, nbytes: int) -> None:
        """Load and unload small model `key` once in a clean window, so its size is LEARNED (a strict need: it may be refused)."""
        from ..residency.holdings import Pool, Source

        self.load_small(key, nbytes)
        self.probes.small_bytes -= nbytes
        self.small.mark_unloaded(key)
        assert key not in self.arbiter.holdings(Pool.SMALL)
        learned = self.arbiter._learned[(Pool.SMALL, key)]
        assert learned.device_bytes == nbytes and learned.source is not Source.DECLARED, learned

    def oom(self, nbytes: int) -> BaseException:
        return torch.OutOfMemoryError(f"CUDA out of memory. Tried to allocate {nbytes} B")

    def assert_outputs(self, test: unittest.TestCase, outputs: dict[str, Any]) -> None:
        for name, value in outputs.items():
            test.assertTrue(torch.equal(value, self.expected[name]), f"{name} differs from the fully resident reference")


@unittest.skipIf(torch is None, "torch not installed")
class RunTests(unittest.TestCase):
    def setUp(self) -> None:
        from . import _mechanics_test_fixtures as fx

        fx.COMPUTE_LOG.clear()

    def test_max_speed_run_matches_reference_and_stays_warm(self) -> None:
        from ..residency.holdings import Pool

        rig = _Rig(self)
        with rig.acquire() as session:
            self.assertEqual(session.steps_effective, MAX_KEEP)
            rig.assert_outputs(self, rig.run(session))
            self.assertTrue(rig.handle().in_session)
        self.assertFalse(session.degraded)
        for component in ("enc", "dit", "ae"):
            self.assertEqual(rig.tiers(component), {Tier.DEVICE})
        holding = rig.arbiter.holdings(Pool.LARGE)[session.key]
        self.assertTrue(holding.resident)
        self.assertEqual(holding.in_use_count, 0)
        self.assertEqual(holding.footprint.device_bytes, rig.handle().ledger.holdings().device_bytes)
        self.assertEqual(sorted(part.name for part in holding.parts), ["ae", "dit", "enc"])
        self.assertEqual(rig.arbiter.health()["reservations"]["count"], 0)
        self.assertFalse(rig.handle().run_lock.locked())
        # A second run reuses the warm components (no reload).
        loads = rig.handle().components["dit"]
        with rig.acquire() as session:
            rig.assert_outputs(self, rig.run(session))
        self.assertIs(rig.handle().components["dit"], loads)

    def test_half_juggles_dit_and_matches_reference(self) -> None:
        rig = _Rig(self)
        inventory = rig.manager._inventories("toy", rig.spec, {}, {"f32": FormatInfo(True, None, 0, "native", 1)})["f32"]["dit"]
        # Half of the card holds about half of the DiT: HALF must juggle.
        rig.probes.device_total = inventory.bytes_total + 2 * inventory.max_block_bytes()
        with rig.acquire(HALF_RUN) as session:
            denoise = session.plan.phase("denoise").components["dit"]
            self.assertIsNotNone(denoise.resident_blocks)
            assert denoise.resident_blocks is not None
            self.assertLess(denoise.resident_blocks, 6)
            self.assertLessEqual(session.forecast.device_peak, rig.probes.device_total // 2)
            seen: dict[str, Any] = {}

            def look(ctx: Any) -> None:
                seen["dit"] = session.placements()["dit"]

            rig.assert_outputs(self, rig.run(session, denoise=look))
        # The ledger's view while juggling: k blocks on the card, the rest parked.
        placement = seen["dit"]
        self.assertIs(placement.tier, Tier.HOST)
        self.assertEqual((placement.blocks_device, placement.blocks_host, placement.blocks_disk), (denoise.resident_blocks, 6 - denoise.resident_blocks, 0))
        self.assertIs(placement.exec_device, ExecDevice.RUN_DEVICE)
        # RAM «Только на время задачи»: nothing is kept after the run.
        for component in ("enc", "dit", "ae"):
            self.assertEqual(rig.tiers(component), {Tier.DROPPED})
            self.assertIsNone(rig.manager.placements(session.key)[component].tier)

    def test_admission_refused_names_the_short_resource(self) -> None:
        from ..residency.holdings import Pool

        rig = _Rig(self, device_total=64)
        with self.assertRaises(LargeModelRefused) as caught:
            rig.acquire()
        self.assertEqual(caught.exception.shortest_resource, "device")
        self.assertIn("видеопамяти", str(caught.exception))
        self.assertEqual(rig.arbiter.holdings(Pool.LARGE), {})
        self.assertFalse(rig.handle().run_lock.locked())


@unittest.skipIf(torch is None, "torch not installed")
class LadderTests(unittest.TestCase):
    def test_oom_in_phase_without_juggling_goes_to_the_next_vram_step_and_learns_a_floor(self) -> None:
        rig = _Rig(self)
        failures = [rig.oom(ACT)]

        def fail_once(ctx: Any) -> None:
            if failures:
                raise failures.pop(0)

        with rig.acquire() as session:
            rig.assert_outputs(self, rig.run(session, denoise=fail_once))
        self.assertEqual(rig.calls["denoise"], 2)
        self.assertEqual(session.steps_requested, AUTO_STEPS)
        # A device failure lowers the VRAM step only; the RAM step is kept.
        self.assertEqual(session.steps_effective, PHASE_KEEP)
        self.assertTrue(session.degraded)
        self.assertEqual(session.degrade_reason, "device_oom")
        self.assertEqual([rung.kind for rung in session.history], [RungKind.NEXT_VRAM_STEP])
        self.assertEqual([(entry.vram, entry.ram) for entry in rig.manager.floors()], [(VramStep.PHASE, None)])
        self.assertTrue(rig.manager.status()[0]["degraded"])
        with rig.acquire() as again:
            self.assertEqual(again.steps_effective, PHASE_KEEP)
            rig.run(again)
        self.assertEqual(rig.manager.reset_floors("toy"), 1)
        with rig.acquire() as fresh:
            self.assertEqual(fresh.steps_effective, MAX_KEEP)
            rig.run(fresh)

    def test_oom_at_load_goes_to_a_gentler_vram_step(self) -> None:
        rig = _Rig(self)
        rig.meta_failures["dit"] = [rig.oom(ACT)]
        with rig.acquire() as session:
            session.load("dit", "ae")
            self.assertEqual(session.history[-1].kind, RungKind.NEXT_VRAM_STEP)
            self.assertEqual(session.history[-1].failure.step, "load:dit")
            rig.assert_outputs(self, rig.run(session))
        self.assertNotEqual(session.steps_effective.vram, VramStep.MAX)
        self.assertIs(session.steps_effective.ram, RamStep.KEEP)

    def test_oom_at_transition_retries_the_transition_on_a_gentler_plan(self) -> None:
        from . import _mechanics_test_fixtures as fx

        class FailingTransport(fx.FakeTransport):
            armed: list[BaseException] = []

            def to_device(self, tensor: Any) -> Any:
                if FailingTransport.armed:
                    raise FailingTransport.armed.pop(0)
                return super().to_device(tensor)

        # The encoder may not run on the CPU, so MAX (everything on the card)
        # does not fit PHASE's peak and PHASE plans its own placement.
        rig = _Rig(self, transport_cls=FailingTransport, encoder_cpu=False)
        peaks = rig.peaks()
        rig.probes.device_total = peaks[PHASE_KEEP]
        with rig.acquire(PHASE_KEEP) as session:
            outputs = {"enc": session.run_phase("encode", rig.phase_fn("encode", "enc"))}
            self.assertEqual(rig.tiers("dit"), {Tier.HOST})
            FailingTransport.armed.append(rig.oom(ACT))
            outputs["dit"] = session.run_phase("denoise", rig.phase_fn("denoise", "dit"))
            outputs["ae"] = session.run_phase("decode", rig.phase_fn("decode", "ae"))
        rig.assert_outputs(self, outputs)
        rung = session.history[-1]
        self.assertEqual(rung.failure.step, "encode->denoise")
        self.assertIs(rung.kind, RungKind.NEXT_VRAM_STEP)
        self.assertEqual(session.steps_effective, HALF_KEEP)

    def test_oom_in_juggled_phase_first_tightens_the_device_room(self) -> None:
        rig = _Rig(self)
        inventory = rig.manager._inventories("toy", rig.spec, {}, {"f32": FormatInfo(True, None, 0, "native", 1)})["f32"]["dit"]
        rig.probes.device_total = inventory.bytes_total + 2 * inventory.max_block_bytes()
        block = inventory.max_block_bytes()
        failures = [rig.oom(block)]

        def fail_once(ctx: Any) -> None:
            if failures:
                raise failures.pop(0)

        with rig.acquire(HALF_RUN) as session:
            before = session.plan.phase("denoise").components["dit"].resident_blocks
            rig.assert_outputs(self, rig.run(session, denoise=fail_once))
            after = session.plan.phase("denoise").components["dit"].resident_blocks
        assert before is not None and after is not None
        self.assertEqual([rung.kind for rung in session.history], [RungKind.TIGHTER_DEVICE])
        self.assertLess(after, before)
        self.assertEqual(session.steps_effective, HALF_RUN)
        self.assertEqual(rig.manager.floors(), [])

    def test_fallback_of_the_failing_phase_reaches_the_phase_function(self) -> None:
        rig = _Rig(self, fallbacks=(Fallback("tiled", frozenset({"decode"}), lambda options: {**options, "tiled": True}),))
        failures = [rig.oom(ACT)]
        seen: list[Any] = []

        def fail_once(ctx: Any) -> None:
            seen.append(ctx.options.get("tiled", False))
            if failures:
                raise failures.pop(0)

        with rig.acquire() as session:
            rig.assert_outputs(self, rig.run(session, decode=fail_once))
        self.assertEqual(seen, [False, True])
        self.assertEqual([rung.kind for rung in session.history], [RungKind.FALLBACK])
        self.assertEqual(session.steps_effective, MAX_KEEP)

    def test_small_idle_model_is_evicted_before_any_degrade(self) -> None:
        rig = _Rig(self)
        small_bytes = 64 * 1024
        peaks = rig.peaks()
        rig.probes.device_total = peaks[MAX_KEEP] + small_bytes
        unloaded = rig.load_small("ocr", small_bytes)
        failures = [rig.oom(small_bytes)]

        def fail_once(ctx: Any) -> None:
            if failures:
                raise failures.pop(0)

        with rig.acquire() as session:
            self.assertEqual(session.steps_effective, MAX_KEEP)
            self.assertEqual(unloaded, [])
            rig.assert_outputs(self, rig.run(session, denoise=fail_once))
        self.assertEqual(unloaded, ["ocr"])
        self.assertEqual([rung.kind for rung in session.history], [RungKind.RECLAIM])
        self.assertFalse(session.degraded)
        self.assertEqual(session.steps_effective, MAX_KEEP)
        self.assertEqual(rig.manager.floors(), [])

    def test_ladder_replans_under_the_device_cap(self) -> None:
        """A degrade re-plan keeps the per-handle ceiling: no rung plans above it."""
        from .degrade import Replan

        rig = _Rig(self)
        report = rig.manager.profiles("toy", {}, SHAPE, device="cuda:0")
        rows = {step: row for step, row in report.vram_rows.items() if isinstance(row, Planned)}
        cap = rows[VramStep.PHASE].forecast.device_peak
        floor = rows[VramStep.MIN].forecast.device_peak
        failures = [rig.oom(ACT)]

        def fail_once(ctx: Any) -> None:
            if failures:
                raise failures.pop(0)

        with rig.acquire(device_cap_bytes=cap) as session:
            self.assertLessEqual(session.forecast.device_peak, cap)
            replanned = session._replan(Replan(MAX_KEEP, {}, 0))
            self.assertTrue(isinstance(replanned, PlanRefusal) or replanned.forecast.device_peak <= cap)
            rig.assert_outputs(self, rig.run(session, denoise=fail_once))
            self.assertTrue(session.degraded)
            self.assertTrue(all(rung.planned is None or rung.planned.forecast.device_peak <= cap for rung in session.history))
            self.assertLessEqual(session.forecast.device_peak, cap)
        # Directly: the session's re-plan sees its own cap (MAX cannot fit under the MIN row's peak).
        with rig.acquire(device_cap_bytes=floor) as tight:
            self.assertIsInstance(tight._replan(Replan(MAX_KEEP, {}, 0)), PlanRefusal)

    def test_retry_cap(self) -> None:
        from ..residency.holdings import Pool

        rig = _Rig(self)
        small_bytes = 64 * 1024
        peaks = rig.peaks()
        rig.probes.device_total = peaks[MAX_KEEP] + 4 * small_bytes
        unloaded: list[str] = []
        for index in range(4):
            unloaded += rig.load_small(f"ocr{index}", small_bytes)

        def always(ctx: Any) -> None:
            raise rig.oom(small_bytes)

        with self.assertRaises(LargeModelRunFailed) as caught:
            with rig.acquire() as session:
                rig.run(session, denoise=always)
        self.assertEqual(caught.exception.retries, 3)
        self.assertEqual(rig.calls["denoise"], 4)
        self.assertEqual([rung.kind for rung in session.history], [RungKind.RECLAIM] * 3)
        # A run ended by a memory failure leaves nothing behind.
        self.assertEqual(rig.handle().ledger.holdings().device_bytes, 0)
        self.assertNotIn(session.key, rig.arbiter.holdings(Pool.LARGE))

    def test_host_short_at_load_is_a_ladder_failure_never_an_allocation(self) -> None:
        rig = _Rig(self)
        with self.assertRaises(LargeModelRunFailed) as caught:
            # MIN/KEEP runs the encoder on the CPU from RAM.
            with rig.acquire(MIN_KEEP) as session:
                rig.probes.host_available = 0
                session.load("enc")
        self.assertIs(caught.exception.failure.kind, FailureKind.HOST_SHORT)
        self.assertNotIn("enc", rig.handle().components)
        # A host failure walks only the RAM axis: the VRAM step stays MIN.
        self.assertTrue(all(rung.planned is None or rung.planned.plan.steps.vram is VramStep.MIN for rung in session.history))

    def test_host_short_mid_transition_drops_the_remaining_units(self) -> None:
        rig = _Rig(self)
        host_at_transition: dict[str, int] = {}

        def starve(ctx: Any) -> None:
            transport = rig.handle().transport
            transport.reset_peaks()
            host_at_transition["start"] = transport.host_bytes()
            rig.probes.host_available = 0

        def check(ctx: Any) -> None:
            transport = rig.handle().transport
            self.assertLessEqual(transport.peak_host, host_at_transition["start"])
            self.assertEqual(rig.tiers("dit"), {Tier.DROPPED})
            rig.probes.host_available = HUGE

        with rig.acquire(PHASE_KEEP) as session:
            self.assertIs(session.plan.phase("decode").components["dit"].tier, Tier.HOST)
            rig.assert_outputs(self, rig.run(session, denoise=starve, decode=check))
        self.assertEqual(len(session.dropped_instead), 6 + 4)
        # Holding less than planned is safe: the DiT is not re-read into RAM
        # just to rest there; the next run reads it when it needs it.
        self.assertEqual(rig.tiers("dit"), {Tier.DROPPED})
        rig.calls.clear()
        with rig.acquire(PHASE_KEEP) as again:
            rig.assert_outputs(self, rig.run(again))
        self.assertEqual(rig.tiers("dit"), {Tier.HOST})

    def test_host_short_before_disk_staging_is_a_host_rung_and_allocates_nothing(self) -> None:
        """The budget shrinks after admission: the hook set refuses its staging buffer (the live guard), the RAM axis answers."""
        from . import _mechanics_test_fixtures as fx

        class Counting(fx.FakeTransport):
            def __init__(self) -> None:
                super().__init__()
                self.staging_calls = 0

            def alloc_staging(self, nbytes: int) -> Any:
                self.staging_calls += 1
                return super().alloc_staging(nbytes)

        rig = _Rig(self, transport_cls=Counting)
        inventory = rig.manager._inventories("toy", rig.spec, {}, {"f32": FormatInfo(True, None, 0, "native", 1)})["f32"]["dit"]
        rig.probes.device_total = inventory.bytes_total + 2 * inventory.max_block_bytes()

        def starve(ctx: Any) -> None:
            # Enough for the resident part's load (one file tensor's transient), never for the staging.
            rig.probes.host_available = inventory.load_host_bytes
            self.assertLess(inventory.load_host_bytes, 2 * inventory.max_block_bytes())

        with self.assertRaises(LargeModelRunFailed) as caught:
            with rig.acquire(Steps(VramStep.HALF, RamStep.MIN)) as session:
                denoise = session.plan.phase("denoise").components["dit"]
                self.assertIs(denoise.tier, Tier.DISK)
                self.assertIsNotNone(denoise.resident_blocks)
                rig.run(session, encode=starve)
        failure = caught.exception.failure
        self.assertIs(failure.kind, FailureKind.HOST_SHORT)
        self.assertEqual(failure.step, "denoise")
        self.assertEqual(rig.handle().transport.staging_calls, 0, "no staging buffer after the guard refused it")
        self.assertNotIn("denoise", rig.calls, "the phase function never ran without its staging")
        # A host failure walks only the RAM axis: the VRAM step stays HALF.
        self.assertTrue(all(rung.planned is None or rung.planned.plan.steps.vram is VramStep.HALF for rung in session.history))

    def test_sysmem_spill_degrades_the_remaining_phases(self) -> None:
        verdicts = [SpillVerdict(False, 0, 0), SpillVerdict(True, 4096, 0), SpillVerdict(False, 0, 0)]
        rig = _Rig(self, spill_detector=lambda before, after, peak: verdicts.pop(0))
        with rig.acquire() as session:
            rig.assert_outputs(self, rig.run(session))
        self.assertEqual(rig.calls, {"encode": 1, "denoise": 1, "decode": 1})
        self.assertEqual(session.hints, ["nvidia_sysmem_fallback"])
        self.assertIs(session.history[-1].failure.kind, FailureKind.SYSMEM_SPILL)
        self.assertIsNot(session.steps_effective.vram, VramStep.MAX)
        self.assertIs(session.steps_effective.ram, RamStep.KEEP)


@unittest.skipIf(torch is None, "torch not installed")
class PoolTests(unittest.TestCase):
    def _warm(self, rig: _Rig) -> Any:
        with rig.acquire() as session:
            rig.run(session)
        return rig.handle()

    def test_idle_large_component_is_parked_for_a_small_admission(self) -> None:
        rig = _Rig(self)
        handle = self._warm(rig)
        held = handle.ledger.holdings().device_bytes
        need = 2 * 1024
        rig.probes.device_total = held + need - 1
        lease = rig.small.begin_model_use("ocr", None, owner="ocr", hint=_hint(need))
        lease.mark_load_failed()
        lease.release()
        tiers = {name: rig.tiers(name) for name in ("enc", "dit", "ae")}
        self.assertIn({Tier.HOST}, tiers.values())
        self.assertNotIn({Tier.DROPPED}, tiers.values())
        self.assertLess(handle.ledger.holdings().device_bytes, held)

    def test_idle_large_component_is_dropped_when_ram_is_short(self) -> None:
        rig = _Rig(self)
        handle = self._warm(rig)
        held = handle.ledger.holdings().device_bytes
        need = 2 * 1024
        rig.probes.device_total = held + need - 1
        rig.probes.host_available = 0
        lease = rig.small.begin_model_use("ocr", None, owner="ocr", hint=_hint(need))
        lease.mark_load_failed()
        lease.release()
        self.assertLess(len(handle.components), 3)
        self.assertEqual(handle.ledger.holdings().host_bytes, 0)

    def test_in_use_handle_is_never_offered_and_its_reservation_protects_the_phase(self) -> None:
        from ..residency.arbiter import ModelMemoryShortError

        rig = _Rig(self)
        slack = 8 * 1024
        rig.probes.device_total = rig.peaks()[MAX_KEEP] + slack
        # In the denoise phase the device has ACT + slack free (the fake allocates
        # no activations) and the run still reserves ACT for its remaining peak.
        fits_raw_only = slack + 1
        needs_reclaim = ACT + slack + 1
        # Only a MEASURED small size may be refused: measure both keys first.
        rig.measure_once("ocr", fits_raw_only)
        rig.measure_once("ocr2", needs_reclaim)
        results: dict[str, Any] = {}

        def contend(ctx: Any) -> None:
            results["reserved"] = rig.arbiter.health()["reservations"]["device_bytes"].get("cuda:0", 0)
            results["free"] = rig.probes.device("cuda:0").free
            held_before = rig.handle().ledger.holdings()
            # Fits the raw probe, not after the running phase's reservation.
            with self.assertRaises(ModelMemoryShortError):
                rig.small.begin_model_use("ocr", None, owner="ocr", hint=_hint(fits_raw_only))
            # Needs a reclaim: the in-use large handle is never offered.
            with self.assertRaises(ModelMemoryShortError):
                rig.small.begin_model_use("ocr2", None, owner="ocr2", hint=_hint(needs_reclaim))
            self.assertEqual(rig.handle().ledger.holdings(), held_before)

        with rig.acquire() as session:
            self.assertEqual(session.steps_effective, MAX_KEEP)
            rig.run(session, denoise=contend)
        self.assertEqual((results["free"], results["reserved"]), (ACT + slack, ACT))
        # Idle again: the first admission now fits (the reservation is gone).
        lease = rig.small.begin_model_use("ocr", None, owner="ocr", hint=_hint(fits_raw_only))
        lease.mark_load_failed()
        lease.release()

    def test_auto_counts_reclaim_at_most_one_vram_step_above_the_free_fit(self) -> None:
        rig = _Rig(self)
        peaks = rig.peaks()
        small_bytes = 8192
        # Free room = PHASE's peak - 4 KiB: neither MAX (CPU encoder: DiT + AE +
        # activation) nor PHASE fits, free memory alone fits only HALF. With the
        # idle small model reclaimed MAX fits too, but «Авто» goes only one VRAM
        # step up from the free fit: PHASE (and the same RAM step).
        rig.probes.device_total = peaks[PHASE_KEEP] - 4096 + small_bytes
        unloaded = rig.load_small("ocr", small_bytes)
        with rig.acquire() as session:
            self.assertEqual(session.steps_effective, PHASE_KEEP)
            rig.run(session)
        self.assertEqual(unloaded, ["ocr"])

    def test_explicit_steps_may_reclaim_fully(self) -> None:
        rig = _Rig(self)
        peaks = rig.peaks()
        small_bytes = 256 * 1024
        rig.probes.device_total = peaks[PHASE_KEEP] + small_bytes
        unloaded = rig.load_small("ocr", small_bytes)
        with rig.acquire(MAX_KEEP) as session:
            self.assertEqual(session.steps_effective, MAX_KEEP)
            rig.run(session)
        self.assertEqual(unloaded, ["ocr"])

    def test_between_runs_state_follows_the_steps(self) -> None:
        from ..residency.holdings import Pool

        cases = {MAX_KEEP: {Tier.DEVICE}, Steps(VramStep.MAX, RamStep.MIN): {Tier.DEVICE}, PHASE_KEEP: {Tier.HOST}, PHASE_RUN: {Tier.DROPPED}}
        for steps, expected in cases.items():
            with self.subTest(steps=steps.label):
                rig = _Rig(self)
                with rig.acquire(steps) as session:
                    rig.run(session)
                for component in ("enc", "dit", "ae"):
                    self.assertEqual(rig.tiers(component), expected)
                holdings = rig.arbiter.holdings(Pool.LARGE)
                if expected == {Tier.DROPPED}:
                    self.assertEqual(holdings, {})
                else:
                    held = rig.handle().ledger.holdings()
                    self.assertEqual((holdings[session.key].footprint.device_bytes, holdings[session.key].footprint.host_bytes), (held.device_bytes, held.host_bytes))

    def test_unload_drops_idle_handles(self) -> None:
        from ..residency.holdings import Pool

        rig = _Rig(self)
        handle = self._warm(rig)
        self.assertEqual(rig.manager.unload(), [handle.key])
        self.assertEqual(handle.ledger.holdings().device_bytes, 0)
        self.assertEqual(rig.arbiter.holdings(Pool.LARGE), {})
        self.assertEqual(rig.manager.unload(), [])

    def test_unload_key_drops_exactly_one_idle_handle(self) -> None:
        from ..residency.holdings import Pool

        rig = _Rig(self)
        keys = []
        for device in ("cuda:0", "cuda:1"):
            with rig.manager.acquire("toy", {}, SHAPE, device=device, format="f32") as session:
                rig.run(session)
            keys.append(session.key)
        first, second = (rig.manager._handles[key] for key in keys)
        kept = second.ledger.holdings()
        self.assertTrue(rig.manager.unload_key(keys[0]))
        self.assertEqual(first.ledger.holdings().device_bytes, 0)
        self.assertNotIn(keys[0], rig.arbiter.holdings(Pool.LARGE))
        self.assertEqual(second.ledger.holdings(), kept, "the other handle of the same model is untouched")
        self.assertFalse(rig.manager.unload_key(keys[0]), "nothing left to drop")
        self.assertFalse(rig.manager.unload_key("no such key"))
        # A handle in a session is never touched (its run lock is only tried).
        with rig.manager.acquire("toy", {}, SHAPE, device="cuda:1", format="f32") as session:
            self.assertFalse(rig.manager.unload_key(keys[1]))
            rig.run(session)
        self.assertEqual(second.ledger.holdings(), kept)


@unittest.skipIf(torch is None, "torch not installed")
class HostRunDeviceTests(unittest.TestCase):
    """A `cpu` run device: planned on the host budget, every byte reported and reserved on the host side."""

    def _acquire(self, rig: _Rig, steps: Steps = AUTO_STEPS) -> Any:
        return rig.manager.acquire("toy", {}, SHAPE, device="cpu", format="f32", steps=steps)

    def _record_device_probes(self, rig: _Rig) -> list[str]:
        probed: list[str] = []
        original = rig.probes.device

        def device(name: str) -> DeviceBudget:
            probed.append(name)
            return original(name)

        rig.probes.device = device  # type: ignore[method-assign]
        return probed

    def test_cpu_run_is_admitted_on_the_host_and_matches_the_reference(self) -> None:
        from ..residency.holdings import Pool

        rig = _Rig(self)
        probed = self._record_device_probes(rig)
        reservations: list[Any] = []
        reserve = rig.arbiter.reserve

        def recording(requester: Any, need: Any) -> Any:
            reservations.append(need)
            return reserve(requester, need)

        rig.arbiter.reserve = recording  # type: ignore[method-assign]
        with self._acquire(rig) as session:
            self.assertEqual(session.steps_effective, MAX_KEEP)
            self.assertEqual(session.forecast.device_peak, 0)
            rig.assert_outputs(self, rig.run(session))
        self.assertNotIn("cpu", probed, "a host run device is never probed as a device")
        self.assertTrue(reservations)
        # Every need is host-only; the run's admission reserves the whole forecast growth there.
        self.assertTrue(all(need.device is None and need.device_bytes == 0 for need in reservations), reservations)
        self.assertTrue(any(need.host_bytes > 0 for need in reservations), reservations)
        held = rig.handle().ledger.holdings()
        self.assertGreater(held.device_bytes, 0)
        holding = rig.arbiter.holdings(Pool.LARGE)[session.key]
        self.assertEqual((holding.footprint.device_bytes, holding.footprint.host_bytes), (0, held.device_bytes + held.host_bytes))
        self.assertTrue(all(part.device_bytes == 0 for part in holding.parts))
        self.assertTrue(rig.handle().host_device)
        self.assertEqual(rig.handle().floor_key.device_class, "host")
        entry = rig.manager.status()[0]
        self.assertTrue(entry["host_device"])
        wire = rig.manager.profiles("toy", {}, SHAPE, device="cpu").to_dict()
        self.assertEqual((wire["vram"]["applicable"], wire["vram"]["steps"], wire["vram"]["recommended"]), (False, [], None))
        self.assertTrue(wire["ram"]["applicable"])
        self.assertEqual([row["id"] for row in wire["ram"]["steps"]], ["keep", "run", "min"])
        self.assertTrue(wire["selected"]["fits"])

    def test_cpu_ram_min_runs_phase_by_phase_and_keeps_nothing(self) -> None:
        rig = _Rig(self)
        with self._acquire(rig, Steps(VramStep.AUTO, RamStep.MIN)) as session:
            self.assertEqual(session.steps_effective, Steps(VramStep.PHASE, RamStep.MIN))
            rig.assert_outputs(self, rig.run(session))
        for component in ("enc", "dit", "ae"):
            self.assertEqual(rig.tiers(component), {Tier.DROPPED})

    def test_cpu_run_refused_on_a_short_host(self) -> None:
        rig = _Rig(self, host_available=64)
        with self.assertRaises(LargeModelRefused) as caught:
            self._acquire(rig)
        self.assertEqual(caught.exception.shortest_resource, "host")
        self.assertIn("оперативной памяти", str(caught.exception))

    def test_cpu_load_is_guarded_on_the_host(self) -> None:
        """After admission the RAM is gone: the load of a "device" unit is refused by the live guard, never allocated."""
        rig = _Rig(self)
        with self.assertRaises(LargeModelRunFailed) as caught:
            with self._acquire(rig) as session:
                rig.probes.host_available = 0
                session.load("dit")
        self.assertIs(caught.exception.failure.kind, FailureKind.HOST_SHORT)
        self.assertNotIn("dit", rig.handle().components)


@unittest.skipIf(torch is None, "torch not installed")
class SessionLifecycleTests(unittest.TestCase):
    """`finish` only shrinks (item 2 of the axes plan); `settle` is the explicit load of the between-runs state."""

    @staticmethod
    def _counting_transport() -> Any:
        from . import _mechanics_test_fixtures as fx

        class CountingTransport(fx.FakeTransport):
            """Counts every allocation or copy that brings bytes onto the "device"."""

            def __init__(self) -> None:
                super().__init__()
                self.up = 0

            def to_device(self, tensor: Any) -> Any:
                self.up += 1
                return super().to_device(tensor)

            def alloc_device(self, nbytes: int) -> Any:
                self.up += 1
                return super().alloc_device(nbytes)

        return CountingTransport

    def _count_loads(self, rig: _Rig) -> list[str]:
        loads: list[str] = []
        original = rig.manager._load_fresh

        def counted(handle: Any, component: Any, *args: Any, **kwargs: Any) -> None:
            loads.append(component.name)
            original(handle, component, *args, **kwargs)

        rig.manager._load_fresh = counted  # type: ignore[method-assign]
        return loads

    def test_finish_after_an_early_error_loads_nothing(self) -> None:
        rig = _Rig(self)
        loads = self._count_loads(rig)
        with self.assertRaises(TypeError):
            with rig.acquire():
                raise TypeError("bad argument before any step")
        self.assertEqual(loads, [], "the between-runs state (everything on the card) is not loaded by finish")
        self.assertEqual(rig.handle().components, {})
        self.assertFalse(rig.handle().run_lock.locked())

    def test_finish_never_moves_a_unit_up(self) -> None:
        rig = _Rig(self, transport_cls=self._counting_transport())
        with rig.acquire(MAX_KEEP) as session:
            rig.run(session)
        handle = rig.handle()
        # A small admission makes the pool park one idle component to RAM.
        held = handle.ledger.holdings().device_bytes
        rig.probes.device_total = held + 2 * 1024 - 1
        lease = rig.small.begin_model_use("ocr", None, owner="ocr", hint=_hint(2 * 1024))
        lease.mark_load_failed()
        lease.release()
        parked = [name for name in ("enc", "dit", "ae") if rig.tiers(name) == {Tier.HOST}]
        self.assertTrue(parked)
        self.assertIs(rig.manager.placements(session.key)[parked[0]].tier, Tier.HOST)
        rig.probes.device_total = HUGE
        loads = self._count_loads(rig)
        with self.assertRaises(TypeError):
            with rig.acquire(MAX_KEEP):
                handle.transport.up = 0
                raise TypeError("bad argument")
        self.assertEqual(handle.transport.up, 0, "finish brought nothing onto the device")
        self.assertEqual(loads, [])
        for name in parked:
            self.assertEqual(rig.tiers(name), {Tier.HOST}, f"{name} stays parked: holding less is safe")

    def test_finish_after_a_run_still_parks_to_the_between_runs_state(self) -> None:
        rig = _Rig(self)
        with self.assertRaises(TypeError):
            with rig.acquire(PHASE_KEEP) as session:
                session.run_phase("encode", rig.phase_fn("encode", "enc"))

                def broken(ctx: Any) -> None:
                    raise TypeError("bad argument in denoise")

                session.run_phase("denoise", broken)
        # The DiT was on the card for denoise; PHASE/KEEP parks everything in RAM.
        self.assertEqual(rig.tiers("dit"), {Tier.HOST})
        self.assertEqual(rig.tiers("enc"), {Tier.HOST})

    def test_settle_loads_the_between_runs_state(self) -> None:
        rig = _Rig(self)
        with rig.acquire(MAX_KEEP) as session:
            session.settle()
            self.assertEqual(session.current_phase, "idle")
            for component in ("enc", "dit", "ae"):
                self.assertEqual(rig.tiers(component), {Tier.DEVICE})
                self.assertIs(session.placements()[component].tier, Tier.DEVICE)
            rig.assert_outputs(self, rig.run(session))
        for component in ("enc", "dit", "ae"):
            self.assertEqual(rig.tiers(component), {Tier.DEVICE})


def _hint(nbytes: int) -> Any:
    from ..residency.holdings import FootprintHint

    return FootprintHint(device="cuda:0", device_bytes=nbytes, host_bytes=0)


@unittest.skipIf(torch is None, "torch not installed")
class ReportTests(unittest.TestCase):
    def test_health_and_status_shape(self) -> None:
        rig = _Rig(self)
        self.assertEqual(rig.manager.health(), {"large_models": [], "limits": {"vram_cap": None, "ram_cap": None}})
        with rig.acquire() as session:
            rig.run(session)
            in_run = rig.manager.health()["large_models"][0]
            self.assertTrue(in_run["in_use"])
        entry = rig.manager.health()["large_models"][0]
        self.assertEqual(set(entry), {"model", "key", "format", "device", "host_device", "vram_step", "ram_step", "device_bytes", "host_bytes", "in_use", "degraded", "degrade_reason", "components"})
        self.assertEqual((entry["model"], entry["format"], entry["vram_step"], entry["ram_step"], entry["in_use"], entry["degraded"], entry["host_device"]), ("toy", "f32", "max", "keep", False, False, False))
        self.assertGreater(entry["device_bytes"], 0)
        self.assertEqual(set(entry["components"]), {"enc", "dit", "ae"})
        dit = entry["components"]["dit"]
        self.assertEqual(set(dit), {"tier", "exec_device", "blocks_device", "blocks_host", "blocks_disk", "device_bytes", "host_bytes"})
        self.assertEqual((dit["tier"], dit["exec_device"], dit["blocks_device"], dit["blocks_host"]), ("device", "run_device", 6, 0))
        self.assertEqual(rig.manager.placements("no such key"), {})

    def test_profiles_report(self) -> None:
        rig = _Rig(self)
        report = rig.manager.profiles("toy", {}, SHAPE, device="cuda:0")
        self.assertEqual(report.format, "f32")
        self.assertEqual(report.recommended, MAX_KEEP)
        wire = report.to_dict()
        self.assertEqual(set(wire), {"model", "format", "selected", "vram", "ram", "learned_floor", "limits", "formats", "suggestions"})
        selected = wire["selected"]
        self.assertEqual((selected["vram_requested"], selected["ram_requested"], selected["vram"], selected["ram"], selected["fits"]), ("auto", "auto", "max", "keep", True))
        self.assertEqual(set(selected["forecast"]), {"device_peak", "host_peak", "peak_phase", "time", "calibrated", "device_calibrated"})
        self.assertEqual({line["component"] for line in selected["plan_summary"]}, {"enc", "dit", "ae"})
        self.assertEqual([entry["id"] for entry in wire["vram"]["steps"]], ["max", "phase", "half", "min"])
        self.assertEqual([entry["id"] for entry in wire["ram"]["steps"]], ["keep", "run", "min"])
        self.assertTrue(all(entry["fits"] for entry in wire["vram"]["steps"] + wire["ram"]["steps"]))
        self.assertEqual(set(wire["vram"]["steps"][0]), {"id", "fits", "same_as", "below_floor", "device_peak", "total_s"})
        self.assertEqual(set(wire["ram"]["steps"][0]), {"id", "fits", "same_as", "below_floor", "host_peak", "total_s"})
        peaks = [entry["device_peak"] for entry in wire["vram"]["steps"]]
        self.assertEqual(peaks, sorted(peaks, reverse=True), "the VRAM rows never grow downwards")
        self.assertEqual((wire["vram"]["recommended"], wire["ram"]["recommended"], wire["vram"]["floor"]), ("max", "keep", None))
        self.assertEqual(wire["formats"][0]["id"], "f32")
        self.assertEqual(wire["formats"][0]["fits_best"], {"vram": "max", "ram": "keep"})
        for key in ("vram_cap", "ram_cap", "vram_total", "vram_free", "ram_total", "ram_free", "ram_cgroup_limit", "vram_reserve", "ram_reserve"):
            self.assertIn(key, wire["limits"])
        self.assertNotIn("keep_policy", wire["limits"])
        json.dumps(wire)
        # The rows follow the selection: an explicit RAM step plans the VRAM rows with it.
        explicit = rig.manager.profiles("toy", {}, SHAPE, device="cuda:0", steps=PHASE_RUN).to_dict()
        self.assertEqual((explicit["selected"]["vram"], explicit["selected"]["ram"]), ("phase", "run"))
        self.assertEqual(explicit["vram"]["recommended"], "max")

    def test_profiles_with_another_handles_claim(self) -> None:
        """`claimed_*_bytes`: another handle's admission counted in the room (device, or the host on a host run device)."""
        rig = _Rig(self, device_total=1 << 30, host_available=1 << 30)
        free = rig.manager.profiles("toy", {}, SHAPE, device="cuda:0")
        self.assertIsInstance(free.selected, Planned)
        claimed = rig.manager.profiles("toy", {}, SHAPE, device="cuda:0", claimed_device_bytes=1 << 30)
        self.assertIsInstance(claimed.selected, PlanRefusal)
        assert isinstance(claimed.selected, PlanRefusal)
        self.assertEqual(claimed.selected.shortest_resource, "device")
        on_host = rig.manager.profiles("toy", {}, SHAPE, device="cpu", claimed_device_bytes=1 << 30)
        assert isinstance(on_host.selected, PlanRefusal)
        self.assertEqual(on_host.selected.shortest_resource, "host", "a host device's claim is host memory")
        self.assertIsInstance(rig.manager.profiles("toy", {}, SHAPE, device="cpu").selected, Planned)
        for bad in (-1, True, 1.5):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                rig.manager.profiles("toy", {}, SHAPE, device="cuda:0", claimed_host_bytes=bad)

    def test_device_cap_bounds_this_handles_room(self) -> None:
        """`device_cap_bytes`: a per-handle ceiling on the forecast device peak, in profiles and admissions alike."""
        rig = _Rig(self)
        free = rig.manager.profiles("toy", {}, SHAPE, device="cuda:0")
        assert isinstance(free.selected, Planned)
        max_peak = free.selected.forecast.device_peak
        min_row = free.vram_rows[VramStep.MIN]
        assert isinstance(min_row, Planned)
        min_peak = min_row.forecast.device_peak
        self.assertLess(min_peak, max_peak - 1)
        # A binding cap: the selection and every row stay under it.
        cap = max_peak - 1
        capped = rig.manager.profiles("toy", {}, SHAPE, device="cuda:0", device_cap_bytes=cap)
        assert isinstance(capped.selected, Planned)
        self.assertLessEqual(capped.selected.forecast.device_peak, cap)
        for step, row in capped.vram_rows.items():
            if isinstance(row, Planned):
                self.assertLessEqual(row.forecast.device_peak, cap, step)
        # A cap at the peak changes nothing.
        same = rig.manager.profiles("toy", {}, SHAPE, device="cuda:0", device_cap_bytes=max_peak)
        assert isinstance(same.selected, Planned)
        self.assertEqual(same.selected.forecast, free.selected.forecast)
        # An explicit step that cannot fit under the cap is refused on the device, never lowered behind the caller's back.
        explicit = rig.manager.profiles("toy", {}, SHAPE, device="cuda:0", steps=MAX_KEEP, device_cap_bytes=min_peak)
        assert isinstance(explicit.selected, PlanRefusal)
        self.assertEqual(explicit.selected.shortest_resource, "device")
        with self.assertRaises(LargeModelRefused):
            rig.acquire(MAX_KEEP, device_cap_bytes=min_peak)
        self.assertFalse(rig.handle().run_lock.locked())
        with rig.acquire(device_cap_bytes=min_peak) as session:
            self.assertLessEqual(session.forecast.device_peak, min_peak)
            rig.assert_outputs(self, rig.run(session))
        for bad in (-1, True, 1.5, "1"):
            with self.subTest(bad=bad):
                with self.assertRaises(ValueError):
                    rig.manager.profiles("toy", {}, SHAPE, device="cuda:0", device_cap_bytes=bad)  # type: ignore[arg-type]
                with self.assertRaises(ValueError):
                    rig.acquire(device_cap_bytes=bad)

    def test_set_limits_validates_and_persists(self) -> None:
        store = MemoryLimitsStore()
        rig = _Rig(self, store=store)
        with self.assertRaises(ValueError):
            rig.manager.set_limits(vram_cap=-1, ram_cap=None)
        limits = rig.manager.set_limits(vram_cap=1 << 30, ram_cap=None)
        self.assertEqual(store.read_limits(), limits)
        self.assertEqual(rig.manager.limits(), Limits(1 << 30, None))


@unittest.skipIf(torch is None, "torch not installed")
class FloorPersistenceTests(unittest.TestCase):
    def test_floors_survive_a_restart_and_reset(self) -> None:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        path = limits_store_path(Path(directory.name))
        rig = _Rig(self, store=JsonLimitsStore(path))
        failures = [rig.oom(ACT)]

        def fail_once(ctx: Any) -> None:
            if failures:
                raise failures.pop(0)

        with rig.acquire() as session:
            rig.run(session, denoise=fail_once)
        restarted = _Rig(self, store=JsonLimitsStore(path))
        self.assertEqual([(entry.key.model, entry.key.format, entry.key.device_class, entry.vram, entry.ram) for entry in restarted.manager.floors()], [("toy", "f32", "test_class", VramStep.PHASE, None)])
        report = restarted.manager.profiles("toy", {}, SHAPE, device="cuda:0").to_dict()
        self.assertEqual((report["learned_floor"]["vram"], report["learned_floor"]["ram"]), ("phase", None))
        self.assertEqual(report["vram"]["floor"], "phase")
        self.assertEqual([entry["below_floor"] for entry in report["vram"]["steps"]], [True, False, False, False])
        self.assertEqual(restarted.manager.profiles("toy", {}, SHAPE, device="cuda:0").recommended, PHASE_KEEP)
        self.assertEqual(restarted.manager.reset_floors(), 1)
        self.assertEqual(json.loads(path.read_text(encoding="utf-8"))["floors"], [])


class StoreTests(unittest.TestCase):
    def test_json_store_round_trip(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = limits_store_path(Path(directory))
            store = JsonLimitsStore(path)
            self.assertEqual(store.read_limits(), Limits())
            self.assertEqual(store.read_floors(), [])
            store.write_limits(Limits(1024, 2048))
            entry = FloorEntry(FloorKey("c", "m", "f"), VramStep.HALF, RamStep.RUN, "device_oom", "denoise", 12.5)
            store.write_floors([entry])
            reread = JsonLimitsStore(path)
            self.assertEqual(reread.read_limits(), Limits(1024, 2048))
            self.assertEqual(reread.read_floors(), [entry])
            self.assertFalse(path.with_name(path.name + ".tmp").exists())

    def test_malformed_store_is_moved_aside(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "limits.json"
            path.write_text("{not json", encoding="utf-8")
            store = JsonLimitsStore(path)
            self.assertEqual(store.read_limits(), Limits())
            self.assertTrue(path.with_name("limits.json.corrupt").exists())
            store.write_limits(Limits(1, None))
            self.assertEqual(JsonLimitsStore(path).read_limits(), Limits(1, None))

    def test_invalid_and_old_entries_are_ignored(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "limits.json"
            floors = [{"model": "m"}, {"device_class": "c", "model": "m", "format": "f", "profile": "fast"}, {"device_class": "c", "model": "m", "format": "f", "vram": "nope", "ram": None}, {"device_class": "c", "model": "m", "format": "f", "vram": "half", "ram": None}]
            path.write_text(json.dumps({"version": 1, "limits": {"vram_cap_bytes": -5, "ram_cap_bytes": True, "keep_policy": "host"}, "floors": floors}), encoding="utf-8")
            store = JsonLimitsStore(path)
            with self.assertLogs(level="INFO") as logs:
                self.assertEqual(store.read_limits(), Limits())
                self.assertEqual([(entry.vram, entry.ram) for entry in store.read_floors()], [(VramStep.HALF, None)])
            self.assertTrue(any("keep_policy" in line for line in logs.output))
            self.assertTrue(any("single profile scale" in line for line in logs.output), "the old profile floor is skipped with one INFO line")


class OomBackstopTests(unittest.TestCase):
    def test_writes_on_linux_when_enabled(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "oom_score_adj"
            self.assertTrue(apply_oom_score_backstop(None, path=path, platform="linux"))
            self.assertEqual(path.read_text(encoding="ascii"), "1000\n")
            self.assertTrue(apply_oom_score_backstop({"large_models_oom_score_adj": "yes"}, path=path, platform="linux"))

    def test_disabled_or_other_platform(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "oom_score_adj"
            self.assertFalse(apply_oom_score_backstop({"large_models_oom_score_adj": False}, path=path, platform="linux"))
            self.assertFalse(apply_oom_score_backstop(None, path=path, platform="win32"))
            self.assertFalse(path.exists())

    def test_write_failure_never_raises(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "missing" / "oom_score_adj"
            with self.assertLogs(level="WARNING"):
                self.assertFalse(apply_oom_score_backstop(None, path=path, platform="linux"))


class ImportTests(unittest.TestCase):
    def test_manager_and_degrade_import_without_torch(self) -> None:
        import subprocess
        import sys

        repo_root = Path(__file__).resolve().parents[4]
        code = (
            "import sys\n"
            "import modules.ai_backend.runtime.large_models.manager\n"
            "import modules.ai_backend.runtime.large_models.degrade\n"
            "assert 'torch' not in sys.modules, 'torch imported'\n"
        )
        completed = subprocess.run([sys.executable, "-c", code], cwd=repo_root, capture_output=True, text=True, timeout=60, check=False)
        self.assertEqual(completed.returncode, 0, completed.stderr)


if __name__ == "__main__":
    unittest.main()
