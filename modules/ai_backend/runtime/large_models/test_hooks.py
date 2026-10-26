"""
File: modules/ai_backend/runtime/large_models/test_hooks.py

Purpose:
Contracts of `hooks.apply_block_residency` on a real-torch 6-block toy model:
forward results identical to the fully resident model under the HOST and
DISK tiers (DISK reads a real tiny safetensors file, fragmented and
misaligned layouts included, buffers included), the rest state between
passes, the event protocol (a transport that runs side-stream copies only
when an event is waited on), compute-first prefetch ordering, the learned
prediction, the prefetch-depth bound, removal and the live host guard before
every DISK staging buffer (a budget that shrinks after admission); the DISK
tier over a GGUF file (`GgufDiskTests`: quantized uint8 rows streamed from the
GGUF data start, forward identical to the resident reference).

Notes:
Skips without torch (the GGUF case also without `gguf`). CPU only: the
transports "device" to CPU tensors.
"""

from __future__ import annotations

import importlib.util
import tempfile
import unittest
from pathlib import Path
from typing import Any

try:
    import torch
except ImportError:  # pragma: no cover - the backend may run without torch
    torch = None  # type: ignore[assignment]


def apply_block_residency_for(component: Any, transport: Any = None) -> Any:
    """`apply_block_residency` with a fresh `FakeTransport` unless one is given."""
    from . import _mechanics_test_fixtures as fx
    from .hooks import apply_block_residency

    return apply_block_residency(component, transport if transport is not None else fx.FakeTransport())


@unittest.skipIf(torch is None, "torch not installed")
class HookTests(unittest.TestCase):
    def setUp(self) -> None:
        from . import _mechanics_test_fixtures as fx

        self.fx = fx
        self._dir = tempfile.TemporaryDirectory()
        self.root = Path(self._dir.name)
        self.reference = fx.reference_toy()
        self.state = fx.toy_state(self.reference)
        self.x = torch.randn(5, 8, generator=torch.Generator().manual_seed(2))
        fx.COMPUTE_LOG.clear()

    def tearDown(self) -> None:
        self._dir.cleanup()

    def _load(self, tier: Any, resident: int, *, interleave: bool = False, align: int | None = None, max_in_flight: int = 2) -> Any:
        from .ledger import Ledger
        from .loader import load_component
        from .spec import ExecDevice
        from .tiers import ComponentPlan

        path = self.root / "toy.safetensors"
        if align is None:
            self.fx.write_toy_checkpoint(path, self.reference, interleave=interleave)
        else:
            self.fx.write_safetensors(path, list(self.state.items()), align=align)
        spec = self.fx.toy_component_spec(path)
        inventory = self.fx.toy_inventory(spec)
        self.ledger = Ledger(max_in_flight=max_in_flight)
        plan = ComponentPlan("net", tier, ExecDevice.RUN_DEVICE, resident, tier)
        self.plan = plan
        return load_component(spec, inventory, plan, options=None, weight_format="f32", device="cpu", ledger=self.ledger)

    def _expected(self, order: list[int] | None = None) -> Any:
        with torch.no_grad():
            return self.reference(self.x, order)

    def _assert_passes(self, component: Any, transport: Any, *, passes: int = 3, order: list[int] | None = None, depth: int = 1) -> Any:
        from .hooks import apply_block_residency

        expected = self._expected(order)
        with apply_block_residency(component, transport, prefetch_depth=depth, plan=self.plan) as hooks:
            for _ in range(passes):
                with torch.no_grad():
                    out = component.module(self.x, order)
                self.assertTrue(torch.equal(out, expected))
                self.component_at_rest(component)
            stats = hooks.stats
        return stats

    def component_at_rest(self, component: Any) -> None:
        from .tiers import Tier

        for ref, entries in component.units.items():
            tier = self.ledger.tier(ref)
            for entry in entries:
                tensor = entry.get()
                if tier is Tier.DISK:
                    self.assertTrue(tensor.is_meta, entry.state_key)
                else:
                    self.assertFalse(tensor.is_meta, entry.state_key)
        self.ledger.assert_single_copy()

    def test_host_tier_forward_is_identical(self) -> None:
        from .tiers import Tier

        component = self._load(Tier.HOST, 2)
        parked = {entry.state_key: entry.get() for ref, entries in component.units.items() if self.ledger.tier(ref) is Tier.HOST for entry in entries}
        transport = self.fx.FakeTransport()
        stats = self._assert_passes(component, transport)
        # The parked host tensors are the module's tensors again, untouched.
        for ref, entries in component.units.items():
            for entry in entries:
                if entry.state_key in parked:
                    self.assertEqual(entry.get().data_ptr(), parked[entry.state_key].data_ptr())
        self.assertEqual(stats.visits, 12)
        self.assertEqual(stats.misses, 1)
        self.assertEqual(stats.hits, 11)
        self.assertTrue(self.fx.same_state(component.module, self.state))

    def test_disk_tier_forward_is_identical(self) -> None:
        from .tiers import Tier

        component = self._load(Tier.DISK, 2)
        stats = self._assert_passes(component, self.fx.FakeTransport())
        self.assertEqual(stats.misses, 1)
        self.assertEqual(self.ledger.holdings().host_bytes, 0)

    def test_disk_tier_fragmented_and_misaligned_layouts(self) -> None:
        from .tiers import Tier

        component = self._load(Tier.DISK, 1, interleave=True)
        self._assert_passes(component, self.fx.FakeTransport())
        # Back to back without padding: every f32 after a 3-byte uint8 `flag` is misaligned.
        component = self._load(Tier.DISK, 1, align=1)
        self._assert_passes(component, self.fx.FakeTransport())

    def _record_computes(self, component: Any, transport: Any) -> list[Any]:
        """Forward hooks (registered BEFORE the residency hooks, so they run first) logging which slot each block computed from."""
        handles = []
        for index, block in enumerate(component.module.blocks):
            def hook(module: Any, args: Any, out: Any, index: int = index) -> None:
                with transport.lock:
                    transport.log.append(("compute", f"blocks.{index}", transport.slot_of(module.fc1.weight.data_ptr())))

            handles.append(block.register_forward_hook(hook))
        return handles

    def test_event_protocol_with_deferred_side_stream(self) -> None:
        from .loader import Stream
        from .tiers import Tier

        for tier in (Tier.HOST, Tier.DISK):
            with self.subTest(tier=tier):
                component = self._load(tier, 1)
                transport = self.fx.DeferredTransport()
                handles = self._record_computes(component, transport)
                # Results are right only if every read waited on its copy event.
                self._assert_passes(component, transport)
                for handle in handles:
                    handle.remove()
                used: dict[int, int] = {}
                waited: set[int] = set()
                pending: int | None = None
                side_copies = 0
                for entry in transport.log:
                    if entry[0] == "compute":
                        # blocks.0 is resident (k = 1); every other block must compute from a slot.
                        if entry[1] != "blocks.0":
                            self.assertIsNotNone(entry[2], "a hooked block computed outside its slot")
                        pending = entry[2]
                    elif entry[0] == "record" and entry[1] is Stream.MAIN and pending is not None:
                        used[pending] = entry[2]
                        pending = None
                    elif entry[0] == "wait" and entry[1] is Stream.SIDE:
                        waited.add(entry[2])
                    elif entry[0] == "copy" and entry[1] is Stream.SIDE:
                        side_copies += 1
                        slot = transport.slot_of(entry[2])
                        if slot in used:
                            self.assertIn(used[slot], waited, f"slot {slot} overwritten before waiting on its used event")
                self.assertGreater(side_copies, 0)
                self.assertTrue(used)

    def test_prefetch_is_issued_after_the_current_compute(self) -> None:
        from .hooks import apply_block_residency
        from .loader import Stream
        from .tiers import Tier

        component = self._load(Tier.HOST, 0)
        transport = self.fx.FakeTransport()
        block_of_host_ptr = {buffer.data_ptr(): ref.block for ref, buffer in component.host_buffers.items()}
        timeline: list[tuple[str, str]] = []
        plain_copy = transport.copy

        def copy(dst: Any, src: Any, stream: Any) -> None:
            if stream is Stream.SIDE:
                timeline.append(("copy", block_of_host_ptr[src.data_ptr()]))
            plain_copy(dst, src, stream)

        transport.copy = copy  # type: ignore[method-assign]
        handles = [block.register_forward_hook(lambda module, args, out, index=index: timeline.append(("computed", f"blocks.{index}"))) for index, block in enumerate(component.module.blocks)]
        with apply_block_residency(component, transport, prefetch_depth=1):
            with torch.no_grad():
                for _ in range(2):
                    component.module(self.x)
        for handle in handles:
            handle.remove()
        self.assertEqual(sum(1 for kind, _ in timeline if kind == "computed"), 12)
        copied_blocks = [name for kind, name in timeline if kind == "copy"]
        # The very first block is fetched by its own pre-hook (a miss); every
        # later copy is a prefetch issued right after its predecessor computed.
        for position, (kind, name) in enumerate(timeline):
            if kind != "copy":
                continue
            computed_before = [entry[1] for entry in timeline[:position] if entry[0] == "computed"]
            if not computed_before:
                self.assertEqual(name, "blocks.0")
                continue
            predecessor = f"blocks.{(int(name.split('.')[1]) - 1) % 6}"
            self.assertEqual(computed_before[-1], predecessor, timeline[: position + 1])
        self.assertGreaterEqual(len(set(copied_blocks)), 6)

    def test_prediction_learns_the_forward_order(self) -> None:
        from .tiers import Tier

        component = self._load(Tier.HOST, 0)
        order = [5, 3, 1, 0, 2, 4]
        stats = self._assert_passes(component, self.fx.FakeTransport(), passes=3, order=order)
        # First pass: the inventory-order guess misses; afterwards every visit is a hit.
        self.assertEqual(stats.visits, 18)
        self.assertLessEqual(stats.misses, 6)
        self.assertEqual(stats.hits + stats.misses, 18)
        self.assertEqual(stats.order[-6:], [f"blocks.{index}" for index in order])
        tail_hits = stats.hits - (6 - stats.misses)
        self.assertGreaterEqual(tail_hits, 12)

    def test_prefetch_depth_bounds_slots_and_flight(self) -> None:
        from .tiers import Tier

        for depth in (0, 1, 2):
            with self.subTest(depth=depth):
                component = self._load(Tier.HOST, 0, max_in_flight=depth + 1)
                transport = self.fx.FakeTransport()
                peak = [0]
                begin = self.ledger.begin_copy

                def counting_begin(ref: Any, to: Any, *, staged: bool = False, _begin: Any = begin) -> None:
                    _begin(ref, to, staged=staged)
                    peak[0] = max(peak[0], sum(1 for state in self.ledger.units() if state.transient is not None))

                self.ledger.begin_copy = counting_begin  # type: ignore[method-assign]
                stats = self._assert_passes(component, transport, depth=depth)
                self.assertEqual(len(transport.slots), depth + 1)
                self.assertLessEqual(peak[0], depth + 1)
                if depth == 0:
                    self.assertEqual(stats.prefetches, 0)
                    self.assertEqual(stats.misses, stats.visits)
        component = self._load(Tier.HOST, 0, max_in_flight=1)
        from .hooks import apply_block_residency

        with self.assertRaises(ValueError):
            apply_block_residency(component, self.fx.FakeTransport(), prefetch_depth=1)

    def test_remove_restores_rest_state_and_frees(self) -> None:
        from .hooks import apply_block_residency
        from .tiers import Tier

        component = self._load(Tier.DISK, 3)
        transport = self.fx.FakeTransport()
        before = self.ledger.holdings()
        hooks = apply_block_residency(component, transport)
        self.assertTrue(component.hooks_active)
        self.assertGreater(self.ledger.holdings().device_bytes, before.device_bytes)
        self.assertGreater(self.ledger.holdings().host_bytes, 0)
        with self.assertRaises(RuntimeError):
            apply_block_residency(component, transport)
        with torch.no_grad():
            component.module(self.x)
        hooks.remove()
        hooks.remove()
        self.assertFalse(component.hooks_active)
        self.assertEqual(self.ledger.holdings(), before)
        self.assertEqual(transport.device_bytes(), 0)
        self.assertEqual(transport.host_bytes(), 0)
        for block in component.module.blocks:
            self.assertEqual(len(block._forward_pre_hooks), 0)
            self.assertEqual(len(block._forward_hooks), 0)
        self.component_at_rest(component)

    def test_failed_forward_is_cleaned_up_by_remove(self) -> None:
        from .hooks import apply_block_residency
        from .tiers import Tier

        component = self._load(Tier.HOST, 2)
        hooks = apply_block_residency(component, self.fx.FakeTransport())
        with torch.no_grad(), self.assertRaises(RuntimeError):
            component.module(torch.randn(5, 7))
        hooks.remove()
        self.component_at_rest(component)
        self.assertTrue(all(state.transient is None for state in self.ledger.units()))
        with torch.no_grad():
            self.assertTrue(torch.equal(component.module(self.x), self._expected()))

    def test_invalid_states_are_refused(self) -> None:
        from .hooks import apply_block_residency
        from .ledger import UnitRef
        from .tiers import ComponentPlan, Tier

        component = self._load(Tier.DISK, 2)
        ref = component.block_ref("blocks.5")
        self.ledger.move_without_copy(ref, Tier.DROPPED)
        with self.assertRaises(ValueError):
            apply_block_residency(component, self.fx.FakeTransport())
        component = self._load(Tier.HOST, 2)
        with self.assertRaises(ValueError):
            apply_block_residency(component, self.fx.FakeTransport(), plan=ComponentPlan("net", Tier.HOST, self.plan.exec_device, 3, Tier.HOST))
        embed = next(r for r in component.units if r.tensor == "embed.weight")
        self.assertIsInstance(embed, UnitRef)
        self.ledger.begin_copy(embed, Tier.HOST)
        self.ledger.commit(embed)
        with self.assertRaises(ValueError):
            apply_block_residency(component, self.fx.FakeTransport())

    def test_stored_dtype_differs_from_the_meta_module(self) -> None:
        """A checkpoint dtype other than the meta module's (FLUX.3: bf16 file, float32 meta build) is the stored layout."""
        from .ledger import Ledger
        from .loader import load_component
        from .spec import ExecDevice
        from .tiers import ComponentPlan, Tier

        with torch.no_grad():
            for block in self.reference.blocks:
                block.gain.copy_(block.gain.to(torch.bfloat16).float())
        items = [(key, value.to(torch.bfloat16) if key.endswith(".gain") else value) for key, value in self.fx.toy_state(self.reference).items()]
        path = self.fx.write_safetensors(self.root / "mixed.safetensors", items)
        spec = self.fx.toy_component_spec(path)
        inventory = self.fx.toy_inventory(spec)
        expected = self._expected()
        for tier in (Tier.HOST, Tier.DISK):
            with self.subTest(tier=tier):
                self.ledger = Ledger()
                self.plan = ComponentPlan("net", tier, ExecDevice.RUN_DEVICE, 1, tier)
                component = load_component(spec, inventory, self.plan, options=None, weight_format="f32", device="cpu", ledger=self.ledger)
                self.assertEqual(component.module.blocks[3].gain.dtype, torch.bfloat16)
                gain_slot = next(entry for entry in component.units[component.block_ref("blocks.3")] if entry.state_key.endswith("gain"))
                self.assertEqual(gain_slot.dtype, torch.bfloat16)
                with apply_block_residency_for(component) as hooks:
                    with torch.no_grad():
                        self.assertTrue(torch.equal(component.module(self.x), expected))
                    self.assertGreater(hooks.stats.visits, 0)

    def test_removed_hooks_keep_no_parked_tensor_alive(self) -> None:
        import gc
        import weakref

        from .moves import restore
        from .tiers import Tier

        component = self._load(Tier.HOST, 2)
        transport = self.fx.FakeTransport()
        transport.adopt(component)
        ref = component.block_ref("blocks.4")
        parked = weakref.ref(component.host_buffers[ref])
        hooks = apply_block_residency_for(component, transport)
        with torch.no_grad():
            component.module(self.x)
        hooks.remove()
        restore(component, transport=transport)
        gc.collect()
        # The hook set object is still referenced here, yet the RAM copy is gone.
        self.assertIsNotNone(hooks)
        self.assertIsNone(parked())
        with torch.no_grad():
            self.assertTrue(torch.equal(component.module(self.x), self._expected()))

    def test_no_hooked_blocks_is_an_empty_set(self) -> None:
        from .hooks import apply_block_residency
        from .tiers import Tier

        component = self._load(Tier.HOST, 6 - 1)
        self.ledger.begin_copy(component.block_ref("blocks.5"), Tier.DEVICE)
        self.ledger.commit(component.block_ref("blocks.5"))
        transport = self.fx.FakeTransport()
        with apply_block_residency(component, transport) as hooks:
            with torch.no_grad():
                self.assertTrue(torch.equal(component.module(self.x), self._expected()))
            self.assertEqual(hooks.stats.visits, 0)
        self.assertEqual(transport.slots, [])

    def test_staging_is_guarded_by_the_live_host_watch(self) -> None:
        """Each DISK staging buffer is checked first, counting the ones already allocated."""
        from .hooks import apply_block_residency
        from .tiers import Tier

        component = self._load(Tier.DISK, 2)
        watch = self.fx.Recording()
        with apply_block_residency(component, self.fx.FakeTransport(), plan=self.plan, watch=watch) as hooks:
            staging = hooks._staging[0].numel()
            with torch.no_grad():
                self.assertTrue(torch.equal(component.module(self.x), self._expected()))
        self.assertEqual(watch.needed, [staging, 2 * staging])

    def test_host_short_before_staging_allocates_nothing_more_and_rolls_back(self) -> None:
        """The budget shrinks after admission: the refused staging buffer is never allocated, and nothing stays."""
        from ..memory_budget import HostMemoryShort
        from .hooks import apply_block_residency
        from .tiers import Tier

        class Counting(self.fx.FakeTransport):  # type: ignore[name-defined]
            def __init__(self) -> None:
                super().__init__()
                self.staging_calls = 0

            def alloc_staging(self, nbytes: int) -> Any:
                self.staging_calls += 1
                return super().alloc_staging(nbytes)

        component = self._load(Tier.DISK, 2)
        before = self.ledger.holdings()
        transport = Counting()
        # Ample for the first check (the first staging buffer), empty from the second on.
        shrinking = self.fx.ShortAfter(1)
        with self.assertRaises(HostMemoryShort):
            apply_block_residency(component, transport, plan=self.plan, watch=shrinking.watch())
        self.assertEqual(shrinking.count, 2)
        self.assertEqual(transport.staging_calls, 1)
        self.assertEqual(transport.device_bytes(), 0)
        self.assertEqual(transport.host_bytes(), 0)
        self.assertFalse(component.hooks_active)
        self.assertEqual(self.ledger.holdings(), before)
        for block in component.module.blocks:
            self.assertEqual(len(block._forward_pre_hooks), 0)
        self.component_at_rest(component)
        # A hook set installed afterwards on an ample budget still works.
        with apply_block_residency(component, self.fx.FakeTransport(), plan=self.plan):
            with torch.no_grad():
                self.assertTrue(torch.equal(component.module(self.x), self._expected()))


def _gguf_toy_classes() -> tuple[type, type]:
    """`(QBlock, QNet)`: a toy whose blocks hold one quantized uint8 row buffer and an F32 scale."""

    class QBlock(torch.nn.Module):
        def __init__(self, rows_shape: tuple[int, ...]) -> None:
            super().__init__()
            self.register_buffer("rows", torch.zeros(rows_shape, dtype=torch.uint8))
            self.scale = torch.nn.Parameter(torch.zeros(16))

        def forward(self, x: torch.Tensor) -> torch.Tensor:
            # Every stored byte reaches the output: a wrong data start changes it.
            return x * self.scale + self.rows.to(x.dtype).sum(dim=1).mean() * 0.001

    class QNet(torch.nn.Module):
        def __init__(self, rows_shape: tuple[int, ...], blocks: int) -> None:
            super().__init__()
            self.embed = torch.nn.Parameter(torch.zeros(16))
            self.blocks = torch.nn.ModuleList([QBlock(rows_shape) for _ in range(blocks)])

        def forward(self, x: torch.Tensor) -> torch.Tensor:
            h = x + self.embed
            for block in self.blocks:
                h = block(h)
            return h

    return QBlock, QNet


@unittest.skipIf(torch is None, "torch not installed")
@unittest.skipUnless(importlib.util.find_spec("gguf") is not None, "gguf is not installed")
class GgufDiskTests(unittest.TestCase):
    """The DISK tier reads quantized GGUF rows at the GGUF header's data start."""

    BLOCKS = 4

    def setUp(self) -> None:
        from .. import _gguf_test_fixtures as fx

        self._dir = tempfile.TemporaryDirectory()
        self.addCleanup(self._dir.cleanup)
        root = Path(self._dir.name)
        tensors = [fx.make_tensor("embed", fx.Q.F32, (16,), seed=50)]
        for index in range(self.BLOCKS):
            tensors.append(fx.make_tensor(f"blocks.{index}.rows", fx.Q.Q8_0, (3, 64), seed=index))
            tensors.append(fx.make_tensor(f"blocks.{index}.scale", fx.Q.F32, (16,), seed=10 + index))
        self.path = fx.write_gguf(root / "toy.gguf", tensors, alignment=64)
        _, net_class = _gguf_toy_classes()
        rows_shape = tuple(tensors[1].raw.shape)
        self.build = lambda: net_class(rows_shape, self.BLOCKS)
        self.reference = self.build()
        with torch.no_grad():
            for tensor in tensors:
                if tensor.name.endswith(".rows"):
                    self.reference.get_buffer(tensor.name).copy_(torch.from_numpy(tensor.raw.copy()))
                else:
                    self.reference.get_parameter(tensor.name).copy_(torch.from_numpy(tensor.reference()))
        self.x = torch.randn(5, 16, generator=torch.Generator().manual_seed(4))

    def test_disk_tier_streams_quantized_rows_from_the_gguf_data_start(self) -> None:
        from . import _mechanics_test_fixtures as fx
        from .hooks import apply_block_residency
        from .inventory import build_inventory
        from .ledger import Ledger
        from .loader import load_component
        from .spec import ComponentSpec, ExecDevice, Role
        from .tiers import ComponentPlan, Tier

        def build_meta(options: Any, weight_format: str) -> Any:
            with torch.device("meta"):
                return self.build()

        spec = ComponentSpec("net", Role.DIT, lambda options, weight_format: [self.path], fx.toy_block_of, build_meta=build_meta)
        inventory = build_inventory(spec, [self.path])
        self.assertTrue(all(entry.span.quant_type is not None for ref in inventory.blocks for entry in inventory.blocks[ref] if entry.key.endswith("rows")))
        ledger = Ledger(max_in_flight=2)
        plan = ComponentPlan("net", Tier.DISK, ExecDevice.RUN_DEVICE, 1, Tier.DISK)
        component = load_component(spec, inventory, plan, options=None, weight_format="gguf", device="cpu", ledger=ledger)
        with torch.no_grad():
            expected = self.reference(self.x)
        with apply_block_residency(component, fx.FakeTransport(), plan=plan):
            for _ in range(2):
                with torch.no_grad():
                    self.assertTrue(torch.equal(component.module(self.x), expected))
                # At rest the DISK blocks are meta placeholders of the stored uint8 layout.
                rest = component.module.blocks[self.BLOCKS - 1].rows
                self.assertTrue(rest.is_meta)
                self.assertEqual(rest.dtype, torch.uint8)


if __name__ == "__main__":
    unittest.main()
