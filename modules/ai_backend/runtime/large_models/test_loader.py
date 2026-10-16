"""
File: modules/ai_backend/runtime/large_models/test_loader.py

Purpose:
Contracts of `loader.load_component` / `load_units` on a real-torch toy
model read from a real tiny safetensors file: tiers per plan, ledger
registration, DISK units never read, 1:N conversion, the host guard (a
pre-read check of the largest load transient, per-tensor checks never above
the forecast's host side; DEVICE units counted too on a host run device), and
the failure contract (nothing registered, units back on `meta`).

Notes:
Skips without torch. CPU only: the "device" is the CPU, the tiers are the
ledger's.
"""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

try:
    import torch
except ImportError:  # pragma: no cover - the backend may run without torch
    torch = None  # type: ignore[assignment]


@unittest.skipIf(torch is None, "torch not installed")
class LoaderTests(unittest.TestCase):
    def setUp(self) -> None:
        from . import _mechanics_test_fixtures as fx

        self.fx = fx
        self._dir = tempfile.TemporaryDirectory()
        self.root = Path(self._dir.name)
        self.reference = fx.reference_toy()
        self.state = fx.toy_state(self.reference)
        self.path = fx.write_toy_checkpoint(self.root / "toy.safetensors", self.reference)
        self.spec = fx.toy_component_spec(self.path)
        self.inventory = fx.toy_inventory(self.spec)
        self.x = torch.randn(3, 8, generator=torch.Generator().manual_seed(1))

    def tearDown(self) -> None:
        self._dir.cleanup()

    def _plan(self, tier: object, resident: int | None) -> object:
        from .spec import ExecDevice
        from .tiers import ComponentPlan

        return ComponentPlan("net", tier, ExecDevice.RUN_DEVICE, resident, tier)  # type: ignore[arg-type]

    def _load(self, tier: object, resident: int | None, **kwargs: object) -> object:
        from .ledger import Ledger
        from .loader import load_component

        self.ledger = Ledger()
        return load_component(self.spec, self.inventory, self._plan(tier, resident), options=None, weight_format="f32", device="cpu", ledger=self.ledger, **kwargs)  # type: ignore[arg-type]

    def test_device_plan_loads_everything_and_matches_the_reference(self) -> None:
        from .ledger import Holdings
        from .tiers import Tier

        loaded = self._load(Tier.DEVICE, None)
        self.assertTrue(self.fx.same_state(loaded.module, self.state))
        with torch.no_grad():
            self.assertTrue(torch.equal(loaded.module(self.x), self.reference(self.x)))
        self.assertEqual({state.tier for state in self.ledger.units()}, {Tier.DEVICE})
        self.assertEqual(self.ledger.holdings(), Holdings(self.inventory.bytes_total, 0))
        self.assertFalse(loaded.module.training)
        # Units: non-block tensors first, then the six blocks in order.
        refs = list(loaded.units)
        self.assertEqual([ref.block for ref in refs if ref.block], [f"blocks.{i}" for i in range(6)])
        self.assertEqual(sum(loaded.unit_bytes.values()), self.inventory.bytes_total)

    def test_partial_host_plan(self) -> None:
        from .tiers import Tier

        loaded = self._load(Tier.HOST, 2)
        tiers = {ref.block: self.ledger.tier(ref) for ref in loaded.units if ref.block}
        self.assertEqual(tiers, {f"blocks.{i}": Tier.DEVICE if i < 2 else Tier.HOST for i in range(6)})
        self.assertTrue(all(self.ledger.tier(ref) is Tier.DEVICE for ref in loaded.units if ref.block is None))
        self.assertTrue(self.fx.same_state(loaded.module, self.state))
        held = self.ledger.holdings()
        self.assertEqual(held.device_bytes + held.host_bytes, self.inventory.bytes_total)
        self.assertEqual(held.host_bytes, self.inventory.remainder_bytes(2))

    def test_partial_disk_plan_does_not_read_the_disk_blocks(self) -> None:
        from .tiers import Tier

        loaded = self._load(Tier.DISK, 2)
        assert loaded.report is not None
        self.assertEqual(loaded.report.bytes_read, self.inventory.resident_bytes(2))
        self.assertGreater(loaded.report.left_on_meta, 0)
        for index in range(6):
            meta = loaded.module.blocks[index].fc1.weight.is_meta
            self.assertEqual(meta, index >= 2)
        self.assertEqual(self.ledger.holdings().host_bytes, 0)

    def test_load_units_brings_a_disk_block_back(self) -> None:
        from .loader import load_units
        from .tiers import Tier

        loaded = self._load(Tier.DISK, 2)
        ref = loaded.block_ref("blocks.4")
        load_units(loaded, [ref])
        self.assertIs(self.ledger.tier(ref), Tier.DEVICE)
        block = loaded.module.blocks[4]
        self.assertTrue(torch.equal(block.fc1.weight, self.state["blocks.4.fc1.weight"]))
        self.assertTrue(torch.equal(block.flag, self.state["blocks.4.flag"]))
        with self.assertRaises(ValueError):
            load_units(loaded, [ref])

    def test_load_units_failure_resets_to_meta(self) -> None:
        from .loader import load_units
        from ._mechanics_test_fixtures import ShortAfter
        from .tiers import Tier

        loaded = self._load(Tier.DISK, 2)
        ref = loaded.block_ref("blocks.3")
        with self.assertRaises(Exception):
            load_units(loaded, [ref], watch=ShortAfter(1).watch())
        self.assertIs(self.ledger.tier(ref), Tier.DISK)
        self.assertTrue(all(entry.get().is_meta for entry in loaded.units[ref]))
        self.ledger.assert_single_copy()

    def test_host_guard_stops_the_load_and_registers_nothing(self) -> None:
        from ..memory_budget import HostMemoryShort
        from ._mechanics_test_fixtures import ShortAfter
        from .tiers import Tier

        with self.assertRaises(HostMemoryShort):
            self._load(Tier.HOST, 2, watch=ShortAfter(5).watch())
        self.assertEqual(self.ledger.units(), [])

    def test_host_guard_counts_host_tensors(self) -> None:
        from ._mechanics_test_fixtures import Recording
        from .tiers import Tier

        recorder = Recording()
        self._load(Tier.HOST, 2, watch=recorder.watch())
        spans = [*self.inventory.non_block, *(span for block in self.inventory.blocks.values() for span in block)]
        # One check before the first read (the largest read transient), then one per file tensor.
        self.assertEqual(len(recorder.needed), 1 + len(spans))
        self.assertEqual(recorder.needed[0], self.inventory.load_host_bytes)
        # Never more than the forecast's host side of this load: what stays + one transient.
        bound = self.inventory.remainder_bytes(2) + self.inventory.load_host_bytes
        self.assertTrue(all(0 <= needed <= bound for needed in recorder.needed))
        # Device-bound tensors add nothing that stays; HOST tensors at least their bytes.
        host_spans = {span.key for name in self.inventory.block_names[2:] for span in self.inventory.blocks[name]}
        self.assertTrue(any(needed >= max(span.device_bytes for span in spans if span.key in host_spans) for needed in recorder.needed[1:]))
        # The last read has nothing after it: it asks only for what stays.
        last = max((span for span in spans), key=lambda span: span.span.start)
        self.assertEqual(recorder.needed[-1], last.device_bytes if last.key in host_spans else 0)

    def test_host_run_device_counts_device_tensors_too(self) -> None:
        """A `cpu` / `mps` run device: a DEVICE unit is host memory, so the guard counts every read tensor."""
        from ._mechanics_test_fixtures import Recording
        from .loader import load_units
        from .tiers import Tier

        recorder = Recording()
        loaded = self._load(Tier.DISK, 2, watch=recorder.watch(), device_is_host=True)
        self.assertTrue(loaded.device_is_host)
        # Every read tensor stays (a pre-read check, then one per read tensor).
        read = [*self.inventory.non_block, *(span for name in self.inventory.block_names[:2] for span in self.inventory.blocks[name])]
        self.assertEqual(len(recorder.needed), 1 + len(read))
        self.assertTrue(all(needed >= span.device_bytes for needed, span in zip(recorder.needed[1:], sorted(read, key=lambda span: span.span.start))))
        self.assertTrue(all(needed <= self.inventory.resident_bytes(2) + self.inventory.load_host_bytes for needed in recorder.needed))
        # A unit re-read later is counted the same way (the component remembers the flag).
        recorder.needed.clear()
        ref = loaded.block_ref("blocks.4")
        load_units(loaded, [ref], watch=recorder)
        block = sorted(loaded.unit_spans[ref], key=lambda span: span.span.start)
        self.assertEqual(recorder.needed[0], max(span.load_host_bytes for span in block))
        self.assertEqual(recorder.needed[-1], block[-1].device_bytes, "the last read asks for what stays")
        self.assertTrue(all(needed >= span.device_bytes for needed, span in zip(recorder.needed[1:], block)))

    def test_spec_without_build_meta_or_convert_is_refused(self) -> None:
        from .ledger import Ledger
        from .loader import load_component
        from .tiers import Tier

        bare = self.fx.toy_component_spec(self.path, build_meta=None)
        with self.assertRaises(ValueError):
            load_component(bare, self.inventory, self._plan(Tier.DEVICE, None), options=None, weight_format="f32", device="cpu", ledger=Ledger())  # type: ignore[arg-type]

    def test_one_to_many_conversion_and_tensor_units(self) -> None:
        from .ledger import Ledger
        from .loader import load_component
        from .tiers import Tier

        state = dict(self.state)
        packed = torch.cat([state.pop("embed.weight"), state.pop("embed.bias")[:, None]], dim=1)
        path = self.fx.write_safetensors(self.root / "packed.safetensors", [("embed.wb", packed), *state.items()])

        def state_keys_of(key: str) -> tuple[str, ...]:
            return ("embed.weight", "embed.bias") if key == "embed.wb" else (key,)

        def convert(key: str, tensor: torch.Tensor):  # type: ignore[no-untyped-def]
            if key == "embed.wb":
                yield "embed.weight", tensor[:, :8].contiguous()
                yield "embed.bias", tensor[:, 8].contiguous()
            else:
                yield key, tensor

        multi = self.fx.toy_component_spec(path, state_keys_of=state_keys_of)
        inventory = self.fx.toy_inventory(multi)
        with self.assertRaises(ValueError):
            load_component(multi, inventory, self._plan(Tier.DEVICE, None), options=None, weight_format="f32", device="cpu", ledger=Ledger())  # type: ignore[arg-type]
        spec = self.fx.toy_component_spec(path, state_keys_of=state_keys_of, convert=convert)
        loaded = load_component(spec, inventory, self._plan(Tier.DEVICE, None), options=None, weight_format="f32", device="cpu", ledger=Ledger())  # type: ignore[arg-type]
        self.assertTrue(self.fx.same_state(loaded.module, self.state))
        embed_unit = next(ref for ref in loaded.units if ref.tensor == "embed.wb")
        self.assertEqual([entry.state_key for entry in loaded.units[embed_unit]], ["embed.weight", "embed.bias"])

    def test_block_that_is_not_a_submodule_path_is_refused(self) -> None:
        from .ledger import Ledger
        from .loader import load_component
        from .tiers import Tier

        spec = self.fx.toy_component_spec(self.path, block_of=lambda key: "group" if key.startswith("blocks.") else None)
        inventory = self.fx.toy_inventory(spec)
        with self.assertRaises(ValueError):
            load_component(spec, inventory, self._plan(Tier.DEVICE, None), options=None, weight_format="f32", device="cpu", ledger=Ledger())  # type: ignore[arg-type]


if __name__ == "__main__":
    unittest.main()
