"""
File: modules/ai_backend/runtime/large_models/test_moves.py

Purpose:
Contracts of `moves.transfer_block_wise` / `park` / `restore` / `drop` with a
byte-ledger fake transport: the flat-peak choreography (device peak <=
max(start, end) + one unit, host peak <= max(start, end) + one unit), the
single-copy invariant after every unit (fake pools == ledger holdings), the
host guard switching the remaining outgoing units to DROPPED (never past the
guard; on a host run device also before every restored unit), DISK / DROPPED
units re-read from the real file (with `read_progress`: one `LoadProgress`
stream per component over the disk units only), `DeviceShort`, the
cache release after every evicted unit, and the refusal while hooks are live.

Notes:
Skips without torch. CPU only.
"""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from typing import Any
from unittest import mock

try:
    import torch
except ImportError:  # pragma: no cover - the backend may run without torch
    torch = None  # type: ignore[assignment]


@unittest.skipIf(torch is None, "torch not installed")
class MoveTests(unittest.TestCase):
    def setUp(self) -> None:
        from . import _mechanics_test_fixtures as fx
        from .ledger import Ledger

        self.fx = fx
        self._dir = tempfile.TemporaryDirectory()
        self.root = Path(self._dir.name)
        self.ledger = Ledger()
        self.transport = fx.FakeTransport()
        self.components: list[Any] = []
        self.x = torch.randn(4, 8, generator=torch.Generator().manual_seed(3))
        self.ref_dit = fx.reference_toy(6, seed=10)
        self.ref_ae = fx.reference_toy(6, seed=11)
        self.dit_path = fx.write_toy_checkpoint(self.root / "dit.safetensors", self.ref_dit)
        self.ae_path = fx.write_toy_checkpoint(self.root / "ae.safetensors", self.ref_ae)

    def tearDown(self) -> None:
        self._dir.cleanup()

    def _load(self, name: str, path: Path, tier: Any, resident: int | None = None, *, device_is_host: bool = False) -> Any:
        from .loader import load_component
        from .spec import ExecDevice
        from .tiers import ComponentPlan

        spec = self.fx.toy_component_spec(path, name=name)
        inventory = self.fx.toy_inventory(spec)
        plan = ComponentPlan(name, tier, ExecDevice.RUN_DEVICE, resident, tier)
        component = load_component(spec, inventory, plan, options=None, weight_format="f32", device="cpu", ledger=self.ledger, device_is_host=device_is_host)
        self.transport.adopt(component)
        self.components.append(component)
        return component

    def _checked(self) -> Any:
        """Patch every per-unit checkpoint to also assert fake pools == ledger holdings."""
        from . import moves

        original = moves._Mover.after_unit
        test = self

        def after_unit(mover: Any, component: Any, ref: Any) -> None:
            original(mover, component, ref)
            test.assert_pools_match_ledger(f"after {ref.label}")

        return mock.patch.object(moves._Mover, "after_unit", after_unit)

    def assert_pools_match_ledger(self, context: str) -> None:
        """Fake pools == ledger: device exactly; host = the packed buffers of the HOST units (ledger bytes + alignment padding)."""
        from .loader import packed_layout
        from .tiers import Tier

        held = self.ledger.holdings()
        self.assertEqual(self.transport.device_bytes(), held.device_bytes, f"device pool vs ledger {context}")
        host_units = [(component, ref) for component in self.components for ref in component.refs(tiers=(Tier.HOST,))]
        packed = sum(packed_layout(component.units[ref])[1] for component, ref in host_units)
        self.assertEqual(self.transport.host_bytes(), packed, f"host pool vs packed HOST units {context}")
        self.assertEqual(held.host_bytes, sum(component.unit_bytes[ref] for component, ref in host_units))
        self.assertLessEqual(packed - held.host_bytes, 256 * sum(len(component.units[ref]) for component, ref in host_units))

    def _forward(self, component: Any) -> Any:
        with torch.no_grad():
            return component.module(self.x)

    def _max_unit(self, *components: Any) -> int:
        return max(max(component.unit_bytes.values()) for component in components)

    def test_dit_out_ae_in_keeps_both_peaks_flat(self) -> None:
        from .moves import transfer_block_wise
        from .tiers import Tier

        dit = self._load("dit", self.dit_path, Tier.DEVICE)
        ae = self._load("ae", self.ae_path, Tier.HOST)
        device_start, host_start = self.transport.device_bytes(), self.transport.host_bytes()
        self.transport.reset_peaks()
        outgoing = [(dit, ref) for ref in dit.refs()]
        incoming = [(ae, ref) for ref in ae.refs()]
        with self._checked():
            report = transfer_block_wise(outgoing, incoming, outgoing_to=Tier.HOST, transport=self.transport)
        device_end, host_end = self.transport.device_bytes(), self.transport.host_bytes()
        from .loader import packed_layout

        unit = self._max_unit(dit, ae)
        # On the host a unit occupies its packed buffer (entries 256-aligned).
        host_unit = max(packed_layout(component.units[ref])[1] for component in (dit, ae) for ref in component.units)
        self.assertLessEqual(self.transport.peak_device, max(device_start, device_end) + unit)
        self.assertLessEqual(self.transport.peak_host, max(host_start, host_end) + host_unit)
        # The naive order (everything out, then everything in, or the reverse)
        # would peak at the sum on one side; the interleaving really happened.
        self.assertLess(self.transport.peak_device, device_start + sum(ae.unit_bytes.values()))
        self.assertEqual(device_end, sum(ae.unit_bytes.values()))
        self.assertTrue(all(self.ledger.tier(ref) is Tier.HOST for ref in dit.refs()))
        self.assertTrue(all(self.ledger.tier(ref) is Tier.DEVICE for ref in ae.refs()))
        self.assertGreaterEqual(self.transport.released, len(report.evicted) + 1)
        self.assertEqual(len(report.evicted), len(outgoing))
        self.assertFalse(report.host_short)
        self.assertTrue(torch.equal(self._forward(ae), self.ref_ae(self.x)))
        # And back again.
        with self._checked():
            transfer_block_wise(incoming, outgoing, outgoing_to=Tier.HOST, transport=self.transport)
        self.assertTrue(torch.equal(self._forward(dit), self.ref_dit(self.x)))
        self.assertTrue(self.fx.same_state(dit.module, self.fx.toy_state(self.ref_dit)))

    def test_host_short_drops_the_remaining_units_instead(self) -> None:
        from .moves import park, restore
        from .tiers import Tier

        dit = self._load("dit", self.dit_path, Tier.DEVICE)
        shortage = self.fx.ShortAfter(4)
        host_before = self.transport.host_bytes()
        with self._checked():
            report = park(dit, to=Tier.HOST, transport=self.transport, watch=shortage.watch())
        self.assertTrue(report.host_short)
        self.assertEqual(len(report.evicted) - len(report.dropped_instead), 4)
        parked = [ref for ref in dit.refs() if self.ledger.tier(ref) is Tier.HOST]
        dropped = [ref for ref in dit.refs() if self.ledger.tier(ref) is Tier.DROPPED]
        self.assertEqual(len(parked), 4)
        self.assertEqual(dropped, report.dropped_instead)
        # Only the units admitted before the guard fired ever reached the host.
        self.assertEqual(self.transport.peak_host - host_before, sum(dit.unit_bytes[ref] for ref in parked))
        self.assertEqual(self.transport.device_bytes(), 0)
        # Restore: parked units come from RAM, dropped ones are re-read from the file.
        with self._checked():
            restore(dit, transport=self.transport)
        self.assertTrue(all(self.ledger.tier(ref) is Tier.DEVICE for ref in dit.refs()))
        self.assertTrue(torch.equal(self._forward(dit), self.ref_dit(self.x)))

    def test_host_run_device_guards_every_restored_unit(self) -> None:
        """On a `cpu` / `mps` run device a restored unit's copy is host memory: the guard stops the restore at a unit boundary."""
        from ..memory_budget import HostMemoryShort
        from .moves import restore
        from .tiers import Tier

        dit = self._load("dit", self.dit_path, Tier.HOST, device_is_host=True)
        with self._checked(), self.assertRaises(HostMemoryShort):
            restore(dit, transport=self.transport, watch=self.fx.ShortAfter(2).watch())
        self.ledger.assert_single_copy()
        self.assertTrue(all(state.transient is None for state in self.ledger.units()))
        self.assertEqual(len(dit.refs(tiers=(Tier.DEVICE,))), 2)
        self.assertEqual(self.transport.device_bytes(), sum(dit.unit_bytes[ref] for ref in dit.refs(tiers=(Tier.DEVICE,))))
        self.assert_pools_match_ledger("after the refused restore")
        # On an ample budget the restore completes.
        restore(dit, transport=self.transport, watch=self.fx.Recording().watch())
        self.assertTrue(torch.equal(self._forward(dit), self.ref_dit(self.x)))

    def test_incoming_from_disk_reads_the_file(self) -> None:
        from .moves import transfer_block_wise
        from .tiers import Tier

        dit = self._load("dit", self.dit_path, Tier.DEVICE)
        ae = self._load("ae", self.ae_path, Tier.DISK, resident=0)
        # Non-block weights of a partial plan are on the device already; bring every block in.
        incoming = [(ae, ref) for ref in ae.refs(tiers=(Tier.DISK,))]
        outgoing = [(dit, ref) for ref in dit.refs()]
        with self._checked():
            report = transfer_block_wise(outgoing, incoming, outgoing_to=Tier.DROPPED, transport=self.transport)
        self.assertEqual(len(report.admitted), 6)
        self.assertEqual(self.transport.host_bytes(), 0)
        self.assertTrue(torch.equal(self._forward(ae), self.ref_ae(self.x)))
        self.assertTrue(all(entry.get().is_meta for ref in dit.refs() for entry in dit.units[ref]))

    def test_reread_progress_is_one_stream_over_the_disk_units_only(self) -> None:
        import functools

        from ..safetensors_stream import ByteProgress
        from . import moves
        from .moves import park, restore
        from .tiers import Tier

        dit = self._load("dit", self.dit_path, Tier.DEVICE)
        park(dit, to=Tier.HOST, transport=self.transport, watch=self.fx.ShortAfter(4).watch())
        dropped = [ref for ref in dit.refs() if self.ledger.tier(ref) is Tier.DROPPED]
        self.assertTrue(dropped)
        frames: list[Any] = []
        # Every chunk reported (no throttle), so the aggregation itself is visible.
        unthrottled = functools.partial(ByteProgress, interval_seconds=0.0)
        with mock.patch.object(moves, "ByteProgress", unthrottled):
            restore(dit, transport=self.transport, read_progress=frames.append)
        total = sum(span.file_bytes for ref in dropped for span in dit.unit_spans[ref])
        self.assertGreater(len(frames), len(dropped) - 1)
        self.assertTrue(all(frame.component == "dit" and frame.reread and frame.total_bytes == total for frame in frames))
        self.assertTrue(all(frame.file == self.dit_path and frame.files == (self.dit_path,) for frame in frames))
        done = [frame.done_bytes for frame in frames]
        # One stream across the units: never back to zero between units, 100% exactly once at the end.
        self.assertEqual(done, sorted(done))
        self.assertEqual(done[-1], total)
        self.assertEqual(done.count(total), 1)
        self.assertTrue(torch.equal(self._forward(dit), self.ref_dit(self.x)))

    def test_a_host_only_restore_reports_nothing(self) -> None:
        from .moves import park, restore
        from .tiers import Tier

        dit = self._load("dit", self.dit_path, Tier.DEVICE)
        park(dit, to=Tier.HOST, transport=self.transport)
        frames: list[Any] = []
        restore(dit, transport=self.transport, read_progress=frames.append)
        self.assertEqual(frames, [])

    def test_device_limit_refuses_before_moving(self) -> None:
        from .moves import DeviceShort, transfer_block_wise
        from .tiers import Tier

        dit = self._load("dit", self.dit_path, Tier.DEVICE)
        ae = self._load("ae", self.ae_path, Tier.HOST)
        before = {state.ref: state.tier for state in self.ledger.units()}
        with self.assertRaises(DeviceShort):
            transfer_block_wise([], [(ae, ref) for ref in ae.refs()], outgoing_to=Tier.HOST, transport=self.transport, device_limit=self.transport.device_bytes() + 10)
        self.assertEqual({state.ref: state.tier for state in self.ledger.units()}, before)

    def test_wrong_tiers_and_live_hooks_are_refused(self) -> None:
        from .hooks import apply_block_residency
        from .moves import drop, park, transfer_block_wise
        from .tiers import Tier

        dit = self._load("dit", self.dit_path, Tier.HOST, resident=2)
        with self.assertRaises(ValueError):
            transfer_block_wise([(dit, dit.block_ref("blocks.4"))], [], outgoing_to=Tier.HOST, transport=self.transport)
        with self.assertRaises(ValueError):
            transfer_block_wise([], [(dit, dit.block_ref("blocks.0"))], outgoing_to=Tier.HOST, transport=self.transport)
        with self.assertRaises(ValueError):
            transfer_block_wise([], [], outgoing_to=Tier.DEVICE, transport=self.transport)
        with apply_block_residency(dit, self.transport):
            with self.assertRaises(RuntimeError):
                park(dit, transport=self.transport)
            with self.assertRaises(RuntimeError):
                drop(dit, transport=self.transport)

    def test_drop_forgets_every_copy(self) -> None:
        from .moves import drop
        from .tiers import Tier

        dit = self._load("dit", self.dit_path, Tier.HOST, resident=3)
        released = drop(dit, transport=self.transport)
        self.assertEqual(released, dit.inventory.bytes_total)
        self.assertEqual((self.transport.device_bytes(), self.transport.host_bytes()), (0, 0))
        self.assertEqual({state.tier for state in self.ledger.units()}, {Tier.DROPPED})
        self.assertEqual(self.ledger.holdings().device_bytes + self.ledger.holdings().host_bytes, 0)

    def test_failed_host_allocation_rolls_the_unit_back(self) -> None:
        from .moves import park
        from .tiers import Tier

        dit = self._load("dit", self.dit_path, Tier.DEVICE)
        calls = [0]
        plain = self.transport.alloc_host

        def flaky(nbytes: int) -> Any:
            calls[0] += 1
            if calls[0] == 3:
                raise RuntimeError("host allocation failed")
            return plain(nbytes)

        self.transport.alloc_host = flaky  # type: ignore[method-assign]
        with self.assertRaises(RuntimeError):
            park(dit, to=Tier.HOST, transport=self.transport)
        # Two units are parked, the third stays whole on the device.
        self.ledger.assert_single_copy()
        self.assertTrue(all(state.transient is None for state in self.ledger.units()))
        self.assertEqual(len(dit.refs(tiers=(Tier.HOST,))), 2)
        self.assert_pools_match_ledger("after the failed park")
        self.assertTrue(torch.equal(self._forward_after_restore(dit), self.ref_dit(self.x)))

    def _forward_after_restore(self, component: Any) -> Any:
        from .moves import restore

        restore(component, transport=self.transport)
        return self._forward(component)


if __name__ == "__main__":
    unittest.main()
