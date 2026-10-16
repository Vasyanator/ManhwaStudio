"""
File: modules/ai_backend/runtime/large_models/test_ledger.py

Purpose:
Contracts of `ledger.Ledger`: one tier per unit, transient copies only between
`begin_copy` and `commit` / `release`, the in-flight bound, staging
accounting, `holdings()` and the debug cross-check verdicts.

Notes:
Stdlib only (no torch).
"""

from __future__ import annotations

import unittest

from .ledger import DEBUG_ENV, Holdings, Ledger, LedgerError, UnitRef, debug_enabled, host_private_probe
from .tiers import Tier

A0 = UnitRef("dit", block="blocks.0")
A1 = UnitRef("dit", block="blocks.1")
A2 = UnitRef("dit", block="blocks.2")
E = UnitRef("dit", tensor="embed.weight")
AE = UnitRef("ae", tensor="dec.w")


class _Probe:
    def __init__(self) -> None:
        self.device: int | None = 1000
        self.host: int | None = 5000

    def device_allocated(self) -> int | None:
        return self.device

    def host_private(self) -> int | None:
        return self.host


def _ledger() -> Ledger:
    ledger = Ledger(max_in_flight=2)
    ledger.register(E, 10, Tier.DEVICE)
    ledger.register(A0, 100, Tier.DEVICE)
    ledger.register(A1, 100, Tier.HOST)
    ledger.register(A2, 100, Tier.DISK)
    ledger.register(AE, 40, Tier.DROPPED)
    return ledger


class UnitRefTests(unittest.TestCase):
    def test_exactly_one_of_block_and_tensor(self) -> None:
        with self.assertRaises(ValueError):
            UnitRef("dit")
        with self.assertRaises(ValueError):
            UnitRef("dit", block="b", tensor="t")
        self.assertEqual(A0.label, "dit:blocks.0")
        self.assertEqual(E.label, "dit:embed.weight")


class LedgerTests(unittest.TestCase):
    def test_holdings_count_tier_bytes_only(self) -> None:
        ledger = _ledger()
        self.assertEqual(ledger.holdings(), Holdings(110, 100))
        self.assertEqual(ledger.holdings("ae"), Holdings(0, 0))
        self.assertEqual(ledger.tier(A2), Tier.DISK)

    def test_duplicate_and_unknown_units_are_refused(self) -> None:
        ledger = _ledger()
        with self.assertRaises(LedgerError):
            ledger.register(A0, 1, Tier.HOST)
        with self.assertRaises(LedgerError):
            ledger.tier(UnitRef("dit", block="nope"))
        with self.assertRaises(LedgerError):
            ledger.register(UnitRef("x", block="b"), -1, Tier.HOST)

    def test_copy_then_commit_moves_the_bytes(self) -> None:
        ledger = _ledger()
        ledger.begin_copy(A0, Tier.HOST)
        # Both copies exist while the move is in flight.
        self.assertEqual(ledger.holdings(), Holdings(110, 200))
        ledger.assert_single_copy()
        ledger.commit(A0)
        self.assertEqual(ledger.tier(A0), Tier.HOST)
        self.assertEqual(ledger.holdings(), Holdings(10, 200))

    def test_release_keeps_the_tier(self) -> None:
        ledger = _ledger()
        ledger.begin_copy(A1, Tier.DEVICE)
        self.assertEqual(ledger.holdings(), Holdings(210, 100))
        ledger.release(A1)
        self.assertEqual(ledger.tier(A1), Tier.HOST)
        self.assertEqual(ledger.holdings(), Holdings(110, 100))

    def test_staged_copies_are_not_counted_twice(self) -> None:
        ledger = _ledger()
        ledger.set_staging("hooks:dit", device_bytes=200, host_bytes=0)
        ledger.begin_copy(A1, Tier.DEVICE, staged=True)
        self.assertEqual(ledger.holdings(), Holdings(310, 100))
        with self.assertRaises(LedgerError):
            ledger.commit(A1)
        ledger.release(A1)
        ledger.clear_staging("hooks:dit")
        self.assertEqual(ledger.holdings(), Holdings(110, 100))

    def test_second_copy_in_the_same_tier_or_twice_is_refused(self) -> None:
        ledger = _ledger()
        with self.assertRaises(LedgerError):
            ledger.begin_copy(A0, Tier.DEVICE)
        with self.assertRaises(LedgerError):
            ledger.begin_copy(A0, Tier.DISK)
        ledger.begin_copy(A0, Tier.HOST)
        with self.assertRaises(LedgerError):
            ledger.begin_copy(A0, Tier.HOST)
        with self.assertRaises(LedgerError):
            ledger.commit(A2)
        with self.assertRaises(LedgerError):
            ledger.release(A2)

    def test_in_flight_bound(self) -> None:
        ledger = _ledger()
        ledger.begin_copy(A1, Tier.DEVICE, staged=True)
        ledger.begin_copy(A0, Tier.HOST)
        with self.assertRaises(LedgerError):
            ledger.begin_copy(E, Tier.HOST)
        ledger.commit(A0)
        ledger.begin_copy(E, Tier.HOST)
        ledger.assert_single_copy()

    def test_move_without_copy_only_to_byteless_tiers(self) -> None:
        ledger = _ledger()
        ledger.move_without_copy(A0, Tier.DROPPED)
        self.assertEqual(ledger.holdings(), Holdings(10, 100))
        with self.assertRaises(LedgerError):
            ledger.move_without_copy(A1, Tier.DEVICE)
        ledger.begin_copy(A1, Tier.DEVICE)
        with self.assertRaises(LedgerError):
            ledger.move_without_copy(A1, Tier.DISK)

    def test_forget_component_refuses_units_in_flight(self) -> None:
        ledger = _ledger()
        ledger.begin_copy(A0, Tier.HOST)
        with self.assertRaises(LedgerError):
            ledger.forget_component("dit")
        ledger.release(A0)
        self.assertEqual(ledger.forget_component("dit"), 4)
        self.assertEqual([state.ref for state in ledger.units()], [AE])

    def test_invalid_bound(self) -> None:
        with self.assertRaises(ValueError):
            Ledger(max_in_flight=0)


class CrossCheckTests(unittest.TestCase):
    def test_not_armed_returns_none(self) -> None:
        self.assertIsNone(_ledger().cross_check(device_tolerance=0, host_slack=0, context="t"))

    def test_matching_deltas_pass(self) -> None:
        ledger = _ledger()
        probe = _Probe()
        ledger.arm_cross_check(probe)
        ledger.begin_copy(A0, Tier.HOST)
        ledger.commit(A0)
        probe.device -= 100
        probe.host += 100
        verdict = ledger.cross_check(device_tolerance=0, host_slack=0, context="park")
        assert verdict is not None
        self.assertTrue(verdict.ok, verdict.detail)
        self.assertEqual((verdict.ledger_device_delta, verdict.ledger_host_delta), (-100, 100))

    def test_unaccounted_host_growth_fails(self) -> None:
        ledger = _ledger()
        probe = _Probe()
        ledger.arm_cross_check(probe)
        # The ledger parked 100 B, the process grew by 300 B: a hidden copy.
        ledger.begin_copy(A0, Tier.HOST)
        ledger.commit(A0)
        probe.device -= 100
        probe.host += 300
        verdict = ledger.cross_check(device_tolerance=0, host_slack=10, context="park")
        assert verdict is not None
        self.assertFalse(verdict.ok)
        self.assertIn("host delta", verdict.detail)

    def test_rss_that_does_not_shrink_after_a_restore_passes(self) -> None:
        ledger = _ledger()
        probe = _Probe()
        ledger.arm_cross_check(probe)
        ledger.begin_copy(A1, Tier.DEVICE)
        ledger.commit(A1)
        probe.device += 100
        verdict = ledger.cross_check(device_tolerance=0, host_slack=0, context="restore")
        assert verdict is not None
        self.assertTrue(verdict.ok, verdict.detail)
        self.assertEqual(verdict.ledger_host_delta, -100)

    def test_device_mismatch_fails_and_unknown_figures_are_skipped(self) -> None:
        ledger = _ledger()
        probe = _Probe()
        ledger.arm_cross_check(probe)
        probe.device += 500
        verdict = ledger.cross_check(device_tolerance=100, host_slack=0, context="x")
        assert verdict is not None
        self.assertFalse(verdict.ok)
        probe.device = None
        verdict = ledger.cross_check(device_tolerance=100, host_slack=0, context="x")
        assert verdict is not None
        self.assertTrue(verdict.ok)
        self.assertIsNone(verdict.measured_device_delta)

    def test_debug_switch_and_host_probe(self) -> None:
        import os
        from unittest import mock

        with mock.patch.dict(os.environ, {DEBUG_ENV: "1"}):
            self.assertTrue(debug_enabled())
        with mock.patch.dict(os.environ, {DEBUG_ENV: "0"}):
            self.assertFalse(debug_enabled())
        self.assertEqual(host_private_probe(lambda: {"RssAnon": 7, "RssShmem": 3})(), 10)
        self.assertIsNone(host_private_probe(lambda: {})())


if __name__ == "__main__":
    unittest.main()
