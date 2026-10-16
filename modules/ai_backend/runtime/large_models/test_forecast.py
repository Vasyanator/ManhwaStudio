"""
File: modules/ai_backend/runtime/large_models/test_forecast.py

Purpose:
Unit tests for `forecast.py` on the toy model with round-number tables: phase
peaks (weights + activation), the overall peak = max over phases, transitions
and the idle state, transitions never counting a moving component in both
places, partial residency staging and per-pass copy time (HOST vs DISK), CPU
execution putting the activation on the host, margin and overhead, the
`calibrated` / `device_calibrated` flags judged per phase, the cold / warm
load split, the load transient (a steady reload and the first run's cold
boundaries, host side only), and a host run device folding every device byte
into the host.
"""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from types import MappingProxyType

from ._test_fixtures import GIB, MIB, simple_tables, toy_files, toy_inventories, toy_spec
from .forecast import forecast
from .spec import ExecDevice, RequestShape
from .tiers import ComponentPlan, PhasePlan, RamStep, ResidencyPlan, Steps, Tier, VramStep, derive_transitions

_RUN, _CPU = ExecDevice.RUN_DEVICE, ExecDevice.CPU
_DIT = 8 * GIB + 64 * MIB
_ENC = 4 * GIB
_AE = GIB // 4
_SHAPE = RequestShape(1024, 1024, prompt_tokens=0, steps=4)


def _cp(name: str, tier: Tier, resident: int | None = None, exec_device: ExecDevice = _RUN, keep: Tier = Tier.DROPPED) -> ComponentPlan:
    return ComponentPlan(name, tier, exec_device, resident, keep)


def _plan(phases: dict[str, tuple[ComponentPlan, ...]], idle: tuple[ComponentPlan, ...]) -> ResidencyPlan:
    states = tuple(PhasePlan(name, MappingProxyType({plan.component: plan for plan in plans})) for name, plans in phases.items())
    idle_state = PhasePlan("idle", MappingProxyType({plan.component: plan for plan in idle}))
    steps = Steps(VramStep.MAX, RamStep.KEEP)
    return ResidencyPlan(steps, steps, "big", states, idle_state, derive_transitions((idle_state, *states, idle_state)))


_NOTHING = (_cp("dit", Tier.DROPPED), _cp("encoder", Tier.DROPPED), _cp("ae", Tier.DROPPED))


class ForecastTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls._dir = tempfile.TemporaryDirectory()
        files = {"big": toy_files(Path(cls._dir.name), "big")}
        cls.spec = toy_spec(files)
        cls.inventories = toy_inventories(cls.spec, files)["big"]

    @classmethod
    def tearDownClass(cls) -> None:
        cls._dir.cleanup()

    def _forecast(self, plan: ResidencyPlan, *, tables=None, baseline: int = 2 * GIB):  # type: ignore[no-untyped-def]
        return forecast(self.spec, plan, self.inventories, _SHAPE, tables or simple_tables(), compute_path="native", baseline_rss=baseline)

    def test_inventory_sizes_of_the_toy(self) -> None:
        self.assertEqual(self.inventories["dit"].bytes_total, _DIT)
        self.assertEqual(self.inventories["encoder"].bytes_total, _ENC)
        self.assertEqual(self.inventories["ae"].bytes_total, _AE)

    def test_everything_resident_peaks_are_weights_plus_activation(self) -> None:
        all_device = (_cp("dit", Tier.DEVICE), _cp("encoder", Tier.DEVICE), _cp("ae", Tier.DEVICE))
        result = self._forecast(_plan({"encode": all_device, "denoise": all_device, "decode": all_device}, all_device))
        weights = _DIT + _ENC + _AE
        self.assertEqual(result.phases["encode"].device_peak, weights + GIB // 4)
        self.assertEqual(result.phases["denoise"].device_peak, weights + GIB)
        self.assertEqual(result.phases["decode"].device_peak, weights + GIB // 2)
        self.assertEqual(result.phases["idle"].device_peak, weights)
        self.assertEqual(result.device_peak, weights + GIB)
        self.assertEqual(result.peak_phase, "denoise")
        self.assertEqual(result.device_peak, max(entry.device_peak for entry in (*result.phases.values(), *result.transitions.values())))
        self.assertEqual({entry.host_peak for entry in result.phases.values()}, {2 * GIB}, "nothing on the host but the baseline")
        # The first run reads every component cold: one file tensor's transient at a time.
        transient = max(inventory.load_host_bytes for inventory in self.inventories.values())
        self.assertEqual(result.transitions["idle->encode"].host_peak, 2 * GIB + transient)
        self.assertEqual(result.host_peak, 2 * GIB + transient)
        self.assertEqual(result.time.load_warm_s, 0.0, "everything kept between runs")
        self.assertAlmostEqual(result.time.load_cold_s, weights / (GIB // 2))

    def test_transition_counts_a_moving_component_in_one_place(self) -> None:
        denoise = (_cp("dit", Tier.DEVICE), _cp("encoder", Tier.DROPPED), _cp("ae", Tier.DEVICE))
        decode = (_cp("dit", Tier.HOST), _cp("encoder", Tier.DROPPED), _cp("ae", Tier.DEVICE))
        result = self._forecast(_plan({"encode": decode, "denoise": denoise, "decode": decode}, _NOTHING))
        move = result.transitions["denoise->decode"]
        # Device: the larger side only, never device + host of the DiT summed.
        self.assertEqual(move.device_weights, _DIT + _AE)
        self.assertEqual(move.host_weights, _DIT)
        block = GIB
        self.assertEqual(move.device_peak, _DIT + _AE + block, "one block in flight on top")
        self.assertEqual(move.host_peak, 2 * GIB + _DIT + block)
        self.assertAlmostEqual(move.seconds, _DIT / GIB, msg="park at 1 GiB/s")
        restore = result.transitions["encode->denoise"]
        self.assertAlmostEqual(restore.seconds, _DIT / (2 * GIB), msg="restore at 2 GiB/s, no disk read")
        self.assertEqual(result.phases["decode"].host_weights, _DIT)
        self.assertEqual(result.phases["decode"].device_weights, _AE)

    def test_partial_residency_host_remainder(self) -> None:
        denoise = (_cp("dit", Tier.HOST, resident=6), _cp("encoder", Tier.DROPPED), _cp("ae", Tier.DROPPED))
        result = self._forecast(_plan({"encode": _NOTHING, "denoise": denoise, "decode": _NOTHING}, _NOTHING))
        phase = result.phases["denoise"]
        resident = 64 * MIB + 6 * GIB
        self.assertEqual(phase.device_weights, resident)
        self.assertEqual(phase.device_peak, resident + GIB + 2 * GIB, "activation + (prefetch depth 1 + 1) staging blocks")
        self.assertEqual(phase.host_weights, 2 * GIB)
        self.assertEqual(phase.host_peak, 2 * GIB + 2 * GIB)
        ktokens = 4096 / 1000
        self.assertAlmostEqual(phase.seconds, 4 * (ktokens + 2 * GIB / (4 * GIB)))
        self.assertAlmostEqual(result.time.per_step_s, ktokens + 0.5)
        lines = {line.component: line for line in result.plan_summary}
        self.assertEqual((lines["dit"].blocks_device, lines["dit"].blocks_host, lines["dit"].blocks_disk), (6, 2, 0))

    def test_partial_residency_disk_remainder_streams_per_pass(self) -> None:
        denoise = (_cp("dit", Tier.DISK, resident=6), _cp("encoder", Tier.DROPPED), _cp("ae", Tier.DROPPED))
        result = self._forecast(_plan({"encode": _NOTHING, "denoise": denoise, "decode": _NOTHING}, _NOTHING))
        phase = result.phases["denoise"]
        self.assertEqual(phase.host_weights, 0)
        self.assertEqual(phase.host_peak, 2 * GIB + 2 * GIB, "only the two staging buffers on the host")
        self.assertAlmostEqual(phase.seconds, 4 * (4096 / 1000 + 2 * GIB / (GIB // 2)))
        self.assertEqual({line.component: line for line in result.plan_summary}["dit"].blocks_disk, 2)

    def test_cpu_execution_puts_the_activation_on_the_host(self) -> None:
        encode = (_cp("dit", Tier.DROPPED), _cp("encoder", Tier.HOST, exec_device=_CPU), _cp("ae", Tier.DROPPED))
        result = self._forecast(_plan({"encode": encode, "denoise": _NOTHING, "decode": _NOTHING}, _NOTHING))
        phase = result.phases["encode"]
        self.assertEqual(phase.device_activation, 0)
        self.assertEqual(phase.host_activation, GIB // 4)
        self.assertEqual(phase.host_peak, 2 * GIB + _ENC + GIB // 4)

    def test_margin_and_runtime_overhead(self) -> None:
        all_device = (_cp("dit", Tier.DEVICE), _cp("encoder", Tier.DROPPED), _cp("ae", Tier.DROPPED))
        tables = simple_tables(overhead_gib=0.5, margin_fraction=0.03)
        result = self._forecast(_plan({"encode": all_device, "denoise": all_device, "decode": all_device}, _NOTHING), tables=tables)
        raw = _DIT + GIB
        self.assertEqual(result.phases["denoise"].device_peak, raw + min(GIB, int(raw * 0.03)) + GIB // 2)

    def test_calibrated_flag(self) -> None:
        all_device = (_cp("dit", Tier.DEVICE), _cp("encoder", Tier.DEVICE), _cp("ae", Tier.DEVICE))
        plan = _plan({"encode": all_device, "denoise": all_device, "decode": all_device}, all_device)
        self.assertTrue(self._forecast(plan).calibrated)
        self.assertFalse(self._forecast(plan, tables=simple_tables(calibrated=False)).calibrated)
        self.assertFalse(self._forecast(plan, tables=simple_tables(denoise_measured=False)).calibrated)
        self.assertFalse(self._forecast(plan, tables=simple_tables(calibrated=False)).device_calibrated)

    def test_calibration_is_judged_per_phase(self) -> None:
        # A CPU encoder on an unmeasured entry: the forecast is an estimate for
        # the UI, but every device-executed phase is measured.
        encode = (_cp("dit", Tier.DROPPED), _cp("encoder", Tier.HOST, exec_device=_CPU), _cp("ae", Tier.DROPPED))
        on_device = (_cp("dit", Tier.DEVICE), _cp("encoder", Tier.DROPPED), _cp("ae", Tier.DEVICE))
        plan = _plan({"encode": encode, "denoise": on_device, "decode": on_device}, _NOTHING)
        result = self._forecast(plan, tables=simple_tables(encode_cpu_measured=False))
        self.assertFalse(result.calibrated)
        self.assertTrue(result.device_calibrated)
        self.assertEqual([name for name, entry in result.phases.items() if not entry.calibrated], ["encode"])
        self.assertTrue(all(entry.calibrated for entry in result.transitions.values()), "transitions carry no activation")
        # An unmeasured device-executed phase clears `device_calibrated` only.
        unmeasured = self._forecast(plan, tables=simple_tables(denoise_measured=False))
        self.assertFalse(unmeasured.device_calibrated)
        self.assertEqual([name for name, entry in unmeasured.phases.items() if not entry.calibrated], ["denoise"])

    def test_reload_after_drop_is_read_again(self) -> None:
        use = (_cp("dit", Tier.DROPPED), _cp("encoder", Tier.DROPPED), _cp("ae", Tier.DEVICE))
        dropped = (_cp("dit", Tier.DROPPED), _cp("encoder", Tier.DROPPED), _cp("ae", Tier.DROPPED))
        result = self._forecast(_plan({"encode": use, "denoise": dropped, "decode": use}, _NOTHING))
        self.assertAlmostEqual(result.time.load_cold_s, 2 * _AE / (GIB // 2))
        self.assertAlmostEqual(result.time.load_warm_s, 2 * _AE / (8 * GIB))

    def test_load_transient_on_a_steady_reload_and_on_the_cold_path(self) -> None:
        """A boundary that reads a component from its files carries its load transient on the host; the device side never does."""
        transient = self.inventories["dit"].load_host_bytes
        self.assertGreater(transient, 0)
        use = (_cp("dit", Tier.DEVICE), _cp("encoder", Tier.DROPPED), _cp("ae", Tier.DROPPED))
        result = self._forecast(_plan({"encode": _NOTHING, "denoise": use, "decode": _NOTHING}, _NOTHING))
        # Steady state: dropped between runs, re-read for denoise (a whole-block move dominates in flight).
        reload = result.transitions["encode->denoise"]
        self.assertEqual(reload.host_peak, 2 * GIB + max(GIB, transient))
        self.assertEqual(reload.device_peak, _DIT + GIB, "device: weights + one unit in flight, no load transient")
        # Kept on the device between runs: only the first run reads it, and that boundary carries the transient.
        kept = (_cp("dit", Tier.DEVICE), _cp("encoder", Tier.DROPPED), _cp("ae", Tier.DROPPED))
        warm = self._forecast(_plan({"encode": kept, "denoise": kept, "decode": kept}, kept))
        self.assertEqual(warm.transitions["idle->encode"].host_peak, 2 * GIB + transient)
        self.assertEqual(warm.transitions["idle->encode"].device_peak, _DIT, "the steady device side: nothing moves")
        self.assertEqual(warm.transitions["encode->denoise"].host_peak, 2 * GIB)
        self.assertEqual(warm.host_peak, 2 * GIB + transient)

    def test_host_run_device_folds_every_device_byte_into_the_host(self) -> None:
        """A `cpu` run device: device peaks 0 (no overhead, no margin), the host carries weights + activation."""
        all_device = (_cp("dit", Tier.DEVICE), _cp("encoder", Tier.DEVICE), _cp("ae", Tier.DEVICE))
        denoise = (_cp("dit", Tier.DEVICE), _cp("encoder", Tier.HOST), _cp("ae", Tier.DROPPED))
        plan = _plan({"encode": all_device, "denoise": denoise, "decode": all_device}, all_device)
        tables = simple_tables(overhead_gib=1.0, margin_fraction=0.1)
        result = forecast(self.spec, plan, self.inventories, _SHAPE, tables, compute_path="native", baseline_rss=2 * GIB, host_device=True)
        weights = _DIT + _ENC + _AE
        every = (*result.phases.values(), *result.transitions.values())
        self.assertEqual({entry.device_peak for entry in every}, {0})
        self.assertEqual(result.device_peak, 0)
        self.assertEqual(result.phases["encode"].host_peak, 2 * GIB + weights + GIB // 4)
        self.assertEqual(result.phases["denoise"].host_peak, 2 * GIB + _DIT + _ENC + GIB, "the DEVICE entry's activation, in RAM")
        self.assertEqual(result.phases["denoise"].host_activation, GIB)
        self.assertEqual(result.phases["denoise"].device_activation, 0)
        self.assertEqual(result.phases["idle"].host_peak, 2 * GIB + weights)
        # One combined side per state, plus ONE unit in flight (the encoder's 1 GiB block, restored to the "device").
        move = result.transitions["denoise->decode"]
        self.assertEqual(move.host_peak, 2 * GIB + max(_DIT + _ENC, weights) + GIB)
        self.assertEqual(result.host_peak, 2 * GIB + weights + GIB)
        # The speed is the CPU entry's (the catch-all: 0 s here), never the device's 1 s per ktoken-pass.
        self.assertEqual(result.phases["denoise"].seconds, 0.0)
        self.assertFalse(result.phases["denoise"].calibrated)
        self.assertFalse(result.calibrated)
        self.assertTrue(result.device_calibrated, "no phase executes on a separate device")


if __name__ == "__main__":
    unittest.main()
