"""
File: modules/ai_backend/runtime/large_models/test_gguf_residency.py

Purpose:
Proof that GGUF-quantized weights survive EVERY tier of the large-model
manager: a real tiny GGUF file (written with `.._gguf_test_fixtures`) holding
a toy whose non-block embedding is a `GgufEmbedding` (Q8_0) and whose 4
residency blocks are `GgufLinear`s (Q8_0 and Q4_0, `in = 64`) plus a plain
F32 head, built on `meta` and swapped with `swap_gguf_modules` exactly as a
model package does.

Covered, each on the CPU with `FakeTransport`:
- `loader.load_component` with the DEVICE, HOST (packed) and DISK tiers;
- `hooks.apply_block_residency` running HOST blocks from their packed host
  buffers and streaming DISK blocks from the GGUF data start;
- `moves.park` / `moves.restore` (to HOST packed and back, to DISK and back,
  the DISK re-read going through the GGUF file again);
- a `LargeModelManager` acquire -> load -> run_phase -> finish under RAM KEEP
  (VRAM «phase»: the idle component parks to RAM between phases and runs),
  and a second run restoring the parked components.

After every step: the forward equals the reference built from
`gguf.quants` dequantization (`torch.equal`), the ledger's single-copy
assertion holds, and every swapped module is still a `GgufLinear` /
`GgufEmbedding` with its `quant_type` and a `uint8` `weight` of the file's
row layout (a parked one a view into its unit's packed host buffer).

Notes:
Skips without torch or `gguf`. Files go to a temporary directory the test
removes.
"""

from __future__ import annotations

import importlib.util
import tempfile
import unittest
from pathlib import Path
from typing import Any

try:
    import torch
    from torch import nn
except ImportError:  # pragma: no cover - the backend may run without torch
    torch = None  # type: ignore[assignment]
    nn = None  # type: ignore[assignment]

BLOCKS = 4
WIDTH = 64
VOCAB = 16
OUT = 4

_HAS_DEPS = torch is not None and importlib.util.find_spec("gguf") is not None


def _toy_class() -> type:
    """`GgufToy`: embed (`nn.Embedding`) -> `BLOCKS` x (`h + fc2(gelu(fc1(h)))`) -> `head` (`nn.Linear`).

    Built as a float model; `swap_gguf_modules` turns `embed` and every
    `fc1` / `fc2` into GGUF layers, the reference keeps it float.
    """

    class Block(nn.Module):
        def __init__(self) -> None:
            super().__init__()
            self.fc1 = nn.Linear(WIDTH, WIDTH)
            self.fc2 = nn.Linear(WIDTH, WIDTH, bias=False)

        def forward(self, h: torch.Tensor) -> torch.Tensor:
            return h + self.fc2(torch.nn.functional.gelu(self.fc1(h)))

    class GgufToy(nn.Module):
        def __init__(self) -> None:
            super().__init__()
            self.embed = nn.Embedding(VOCAB, WIDTH)
            self.blocks = nn.ModuleList([Block() for _ in range(BLOCKS)])
            self.head = nn.Linear(WIDTH, OUT)

        def forward(self, ids: torch.Tensor) -> torch.Tensor:
            h = self.embed(ids)
            for block in self.blocks:
                h = block(h)
            return self.head(h)

    return GgufToy


class _GgufToyFile:
    """One GGUF file of the toy, its quant map and its float reference model."""

    def __init__(self, path: Path, *, seed: int) -> None:
        from .. import _gguf_test_fixtures as gfx
        from ..tensor_container import read_container_layout

        Q = gfx.Q
        tensors = [gfx.make_tensor("embed.weight", Q.Q8_0, (VOCAB, WIDTH), seed=seed)]
        for index in range(BLOCKS):
            base = seed + 10 * (index + 1)
            tensors += [
                gfx.make_tensor(f"blocks.{index}.fc1.weight", Q.Q8_0, (WIDTH, WIDTH), seed=base),
                gfx.make_tensor(f"blocks.{index}.fc1.bias", Q.F32, (WIDTH,), seed=base + 1),
                gfx.make_tensor(f"blocks.{index}.fc2.weight", Q.Q4_0, (WIDTH, WIDTH), seed=base + 2),
            ]
        tensors += [gfx.make_tensor("head.weight", Q.F32, (OUT, WIDTH), seed=seed + 1), gfx.make_tensor("head.bias", Q.F32, (OUT,), seed=seed + 2)]
        self.path = gfx.write_gguf(path, tensors, alignment=64)
        self.tensors = {tensor.name: tensor for tensor in tensors}
        layout = read_container_layout(self.path)
        #: `"<module path>.weight"` -> ggml type id, straight from the header (what a model package swaps by).
        self.quant_types = {span.name: span.quant_type for span in layout.spans if span.quant_type is not None}
        self.reference = self._reference()

    def _reference(self) -> Any:
        """The float toy filled with the `gguf.quants` values.

        A quantized weight is the gguf.quants float32 value rounded once to
        float16: diffusers' block functions (called with `dtype=None` by
        `gguf_layers.dequantize`) multiply the f16 scale by the integer quant
        IN float16, and that product is exactly representable in float32, so
        one rounding of the float32 value to float16 is the identical number.
        Everything after the weights is the same torch code, so the GGUF
        model's output must be bit-equal.
        """
        net = _toy_class()()
        with torch.no_grad():
            for name, tensor in self.tensors.items():
                values = torch.from_numpy(tensor.reference().copy())
                if name in self.quant_types:
                    values = values.to(torch.float16).to(torch.float32)
                net.get_parameter(name).copy_(values)
        return net.eval()

    def build_meta(self, options: Any, weight_format: str) -> Any:
        """The toy on `meta` with its quantized submodules swapped (a model package's `build_meta`)."""
        from ..gguf_layers import swap_gguf_modules

        with torch.device("meta"):
            net = _toy_class()()
        replaced = swap_gguf_modules(net, self.quant_types, compute_dtype=torch.float32)
        assert replaced == 1 + 2 * BLOCKS, replaced
        return net


@unittest.skipUnless(_HAS_DEPS, "torch or gguf not installed")
class _GgufCase(unittest.TestCase):
    """Shared checks of the GGUF residency tests."""

    def make_toy(self, name: str, *, seed: int) -> _GgufToyFile:
        if not hasattr(self, "_root"):
            directory = tempfile.TemporaryDirectory()
            self.addCleanup(directory.cleanup)
            self._root = Path(directory.name)
        return _GgufToyFile(self._root / f"{name}.gguf", seed=seed)

    def ids(self, seed: int = 7) -> Any:
        return torch.randint(0, VOCAB, (3, 5), generator=torch.Generator().manual_seed(seed))

    def assert_forward(self, module: Any, toy: _GgufToyFile, context: str) -> None:
        """`module(ids)` equals the reference toy's output bit for bit."""
        ids = self.ids()
        with torch.no_grad():
            out = module(ids)
            expected = toy.reference(ids)
        self.assertEqual(out.dtype, torch.float32, context)
        self.assertTrue(torch.equal(out, expected), f"GGUF output differs from the gguf.quants reference ({context})")

    def assert_gguf_modules(self, component: Any, toy: _GgufToyFile, context: str) -> None:
        """Every swapped module is still a GGUF layer with its quant type and a uint8 weight of the file's row layout.

        A DEVICE / HOST unit's weight holds real bytes (a HOST one is a view
        into its unit's packed host buffer); a DISK / DROPPED unit's weight is
        a `meta` placeholder of the same uint8 layout.
        """
        from ..gguf_layers import GgufEmbedding, GgufLinear
        from .tiers import Tier

        tier_of: dict[str, Any] = {}
        buffer_of: dict[str, Any] = {}
        for ref, entries in component.units.items():
            for entry in entries:
                tier_of[entry.state_key] = component.ledger.tier(ref)
                if ref in component.host_buffers:
                    buffer_of[entry.state_key] = component.host_buffers[ref]
        for key, quant_type in toy.quant_types.items():
            path = key[: -len(".weight")]
            module = component.module.get_submodule(path)
            expected_class = GgufEmbedding if path == "embed" else GgufLinear
            self.assertIs(type(module), expected_class, f"{path} ({context})")
            self.assertEqual(module.quant_type, quant_type, f"{path} ({context})")
            weight = module.weight
            self.assertNotIsInstance(weight, nn.Parameter, f"{path}: the uint8 weight became a parameter ({context})")
            self.assertEqual(weight.dtype, torch.uint8, f"{path} ({context})")
            self.assertEqual(tuple(weight.shape), tuple(toy.tensors[key].raw.shape), f"{path} ({context})")
            tier = tier_of[key]
            self.assertEqual(weight.is_meta, tier in (Tier.DISK, Tier.DROPPED), f"{path} in {tier} ({context})")
            if tier is Tier.HOST:
                buffer = buffer_of[key]
                start = buffer.data_ptr()
                self.assertTrue(start <= weight.data_ptr() < start + buffer.numel(), f"{path}: a HOST weight is not a view of its packed buffer ({context})")
            if not weight.is_meta:
                self.assertTrue(torch.equal(weight.cpu(), torch.from_numpy(toy.tensors[key].raw.copy())), f"{path}: stored bytes changed ({context})")

    def assert_rest(self, component: Any, toy: _GgufToyFile, context: str) -> None:
        """Single copy, GGUF modules intact."""
        component.ledger.assert_single_copy()
        self.assert_gguf_modules(component, toy, context)


class GgufMechanicsTests(_GgufCase):
    """Loader tiers, residency hooks and park / restore over a GGUF file."""

    def setUp(self) -> None:
        self.toy = self.make_toy("toy", seed=0)

    def _spec(self) -> Any:
        from . import _mechanics_test_fixtures as fx
        from .spec import ComponentSpec, Role

        toy = self.toy
        return ComponentSpec("net", Role.DIT, lambda options, weight_format: [toy.path], fx.toy_block_of, build_meta=toy.build_meta)

    def _load(self, tier: Any, resident: int | None) -> tuple[Any, Any, Any]:
        """`(component, plan, transport)`: the toy loaded per a one-tier plan, its tensors adopted by a fresh `FakeTransport`."""
        from . import _mechanics_test_fixtures as fx
        from .inventory import build_inventory
        from .ledger import Ledger
        from .loader import load_component
        from .spec import ExecDevice
        from .tiers import ComponentPlan

        spec = self._spec()
        self.inventory = build_inventory(spec, [self.toy.path])
        plan = ComponentPlan("net", tier, ExecDevice.RUN_DEVICE, resident, tier)
        component = load_component(spec, self.inventory, plan, options=None, weight_format="q8_0", device="cpu", ledger=Ledger(max_in_flight=2))
        transport = fx.FakeTransport()
        transport.adopt(component)
        return component, plan, transport

    def _assert_hooked(self, component: Any, plan: Any, transport: Any, context: str, *, passes: int = 2) -> None:
        from .hooks import apply_block_residency

        with apply_block_residency(component, transport, plan=plan) as hooks:
            for index in range(passes):
                self.assert_forward(component.module, self.toy, f"{context}, pass {index}")
                self.assert_rest(component, self.toy, f"{context}, at rest after pass {index}")
            # Every off-device block went through a hook slot on every pass.
            self.assertEqual(hooks.stats.visits, passes * (BLOCKS - plan.resident_blocks), context)
        self.assert_rest(component, self.toy, f"{context}, hooks removed")

    def test_inventory_sees_quantized_spans(self) -> None:
        from .inventory import build_inventory

        inventory = build_inventory(self._spec(), [self.toy.path])
        spans = [*inventory.non_block, *(span for block in inventory.blocks.values() for span in block)]
        quantized = {span.key: span.span.quant_type for span in spans if span.span.quant_type is not None}
        self.assertEqual(quantized, self.toy.quant_types)
        self.assertEqual(list(inventory.block_names), [f"blocks.{index}" for index in range(BLOCKS)])

    def test_device_tier(self) -> None:
        from .ledger import Holdings
        from .tiers import Tier

        component, _, _ = self._load(Tier.DEVICE, None)
        self.assertEqual({state.tier for state in component.ledger.units()}, {Tier.DEVICE})
        self.assertEqual(component.ledger.holdings(), Holdings(self.inventory.bytes_total, 0))
        self.assert_rest(component, self.toy, "DEVICE load")
        self.assert_forward(component.module, self.toy, "DEVICE load")

    def test_host_tier_packed_and_hooked(self) -> None:
        from .tiers import Tier

        component, plan, transport = self._load(Tier.HOST, 1)
        tiers = {ref.block: component.ledger.tier(ref) for ref in component.units if ref.block}
        self.assertEqual(tiers, {f"blocks.{index}": Tier.DEVICE if index < 1 else Tier.HOST for index in range(BLOCKS)})
        self.assertEqual(len(component.host_buffers), BLOCKS - 1)
        self.assertEqual(component.ledger.holdings().host_bytes, self.inventory.remainder_bytes(1))
        self.assert_rest(component, self.toy, "HOST load")
        # Parked entries are host views the CPU can run directly ...
        self.assert_forward(component.module, self.toy, "HOST load, no hooks")
        # ... and through the residency hooks (the run-device path).
        self._assert_hooked(component, plan, transport, "HOST hooks")

    def test_disk_tier_streams_blocks_from_the_gguf_data_start(self) -> None:
        from .tiers import Tier

        component, plan, transport = self._load(Tier.DISK, 1)
        assert component.report is not None
        self.assertEqual(component.report.bytes_read, self.inventory.resident_bytes(1))
        self.assertEqual(component.ledger.holdings().host_bytes, 0)
        self.assert_rest(component, self.toy, "DISK load")
        self._assert_hooked(component, plan, transport, "DISK hooks")
        self.assertEqual(component.ledger.holdings().host_bytes, 0)

    def test_park_to_host_and_restore(self) -> None:
        from .moves import park, restore
        from .tiers import Tier

        component, _, transport = self._load(Tier.DEVICE, None)
        device_bytes = transport.device_bytes()
        report = park(component, to=Tier.HOST, transport=transport)
        self.assertFalse(report.host_short)
        self.assertEqual({state.tier for state in component.ledger.units()}, {Tier.HOST})
        self.assertEqual(transport.device_bytes(), 0)
        self.assertEqual(set(component.host_buffers), set(component.units))
        self.assert_rest(component, self.toy, "parked to HOST")
        self.assert_forward(component.module, self.toy, "parked to HOST")
        restore(component, transport=transport)
        self.assertEqual({state.tier for state in component.ledger.units()}, {Tier.DEVICE})
        self.assertEqual(component.host_buffers, {})
        self.assertEqual(transport.host_bytes(), 0)
        self.assertEqual(transport.device_bytes(), device_bytes)
        self.assert_rest(component, self.toy, "restored from HOST")
        self.assert_forward(component.module, self.toy, "restored from HOST")

    def test_park_to_disk_and_restore_rereads_the_gguf_file(self) -> None:
        from .moves import park, restore
        from .tiers import Tier

        component, _, transport = self._load(Tier.DEVICE, None)
        park(component, to=Tier.DISK, transport=transport)
        self.assertEqual({state.tier for state in component.ledger.units()}, {Tier.DISK})
        self.assertEqual(component.ledger.holdings().device_bytes + component.ledger.holdings().host_bytes, 0)
        self.assert_rest(component, self.toy, "parked to DISK")
        restore(component, transport=transport)
        self.assertEqual({state.tier for state in component.ledger.units()}, {Tier.DEVICE})
        self.assert_rest(component, self.toy, "restored from DISK")
        self.assert_forward(component.module, self.toy, "restored from DISK")


class _Probes:
    """Budget probes over the fake transports: ample host, a large card whose usage is the transports' device pools."""

    def __init__(self, transports: list[Any]) -> None:
        self.transports = transports
        self.device_total = 1 << 40

    def host(self) -> Any:
        from ..memory_budget import HostBudget

        return HostBudget(total=1 << 40, available=1 << 40, cgroup_limit=None, cgroup_used=None, rss_anon=1 << 20, rss_shmem=0, reserve=0)

    def device(self, device: str) -> Any:
        from ..memory_budget import DeviceBudget

        used = sum(transport.device_bytes() for transport in self.transports)
        return DeviceBudget(device=device, total=self.device_total, free=self.device_total - used, torch_allocated=0, torch_reserved=0, non_torch=used, reserve=0, cap=None)


class GgufManagerRoundTripTests(_GgufCase):
    """A two-component GGUF model (`enc`, `dit`) driven by the real `LargeModelManager` under RAM KEEP."""

    def setUp(self) -> None:
        from . import _mechanics_test_fixtures as fx
        from ..model_manager import SmallModelManager
        from ..residency.arbiter import ResidencyArbiter
        from .manager import LargeModelManager, MemoryLimitsStore
        from .spec import ComponentSpec, ExecDevice, FormatInfo, LargeModelSpec, PhaseSpec, PhaseWork, Role
        from .test_manager import _tables

        self.toys = {"enc": self.make_toy("enc", seed=300), "dit": self.make_toy("dit", seed=600)}
        device_only = frozenset({ExecDevice.RUN_DEVICE})

        def component(name: str, role: Role) -> ComponentSpec:
            toy = self.toys[name]
            return ComponentSpec(name, role, lambda options, weight_format: [toy.path], fx.toy_block_of, exec_devices_allowed=device_only, build_meta=toy.build_meta)

        def work(shape: Any, options: Any) -> Any:
            return PhaseWork(tokens=0, passes=1)

        self.spec = LargeModelSpec(
            family="toy",
            components=(component("enc", Role.ENCODER), component("dit", Role.DIT)),
            phases=(PhaseSpec("encode", frozenset({"enc"}), "enc", work), PhaseSpec("denoise", frozenset({"dit"}), "dit", work)),
            formats=lambda options: {"q8_0": FormatInfo(True, None, 0, "gguf", 1)},
        )
        self.transports: list[Any] = []

        def transport_factory(device: str) -> Any:
            transport = fx.FakeTransport()
            self.transports.append(transport)
            return transport

        probes = _Probes(self.transports)
        arbiter = ResidencyArbiter(probes, admission_wait_s=0.0)
        self.small = SmallModelManager(4, arbiter=arbiter)
        self.manager = LargeModelManager(arbiter, limits_store=MemoryLimitsStore(), tables=_tables(), probes=probes, transport_factory=transport_factory, device_class=lambda device: "test_class")
        self.manager.register(self.spec, model="toy")

    def _acquire(self) -> Any:
        from .spec import RequestShape
        from .tiers import RamStep, Steps, VramStep

        return self.manager.acquire("toy", {}, RequestShape(width=64, height=64), device="cuda:0", format="q8_0", steps=Steps(VramStep.PHASE, RamStep.KEEP))

    def _handle(self) -> Any:
        handles = list(self.manager._handles.values())
        self.assertEqual(len(handles), 1)
        return handles[0]

    def _tiers(self, name: str) -> set[Any]:
        from .tiers import Tier

        handle = self._handle()
        if name not in handle.components:
            return {Tier.DROPPED}
        return {state.tier for state in handle.ledger.units(name)}

    def _assert_all_rest(self, context: str) -> None:
        handle = self._handle()
        handle.ledger.assert_single_copy()
        for name, component in handle.components.items():
            self.assert_gguf_modules(component, self.toys[name], f"{name}, {context}")

    def _phase(self, name: str, other: str, context: str) -> Any:
        """A phase function: checks the residency inside the phase, then runs `name`'s module."""
        from .tiers import Tier

        def fn(ctx: Any) -> Any:
            self.assertEqual(self._tiers(name), {Tier.DEVICE}, f"{name} computes from the device ({context})")
            if other in self._handle().components:
                self.assertIn(self._tiers(other), ({Tier.HOST}, {Tier.DROPPED}), f"{other} idle during {name} ({context})")
            self._assert_all_rest(f"inside {name} ({context})")
            ids = self.ids()
            with torch.no_grad():
                return ctx.module(name)(ids), self.toys[name].reference(ids)

        return fn

    def _run(self, session: Any, context: str) -> None:
        session.load()
        self._assert_all_rest(f"after load ({context})")
        for phase, name, other in (("encode", "enc", "dit"), ("denoise", "dit", "enc")):
            out, expected = session.run_phase(phase, self._phase(name, other, context))
            self.assertTrue(torch.equal(out, expected), f"{name} output differs from the gguf.quants reference ({context})")
            self._assert_all_rest(f"after {phase} ({context})")

    def test_acquire_load_run_finish_parks_and_restores_quantized_weights(self) -> None:
        from .tiers import Tier

        with self._acquire() as session:
            self._run(session, "first run")
            # Under VRAM «phase» the encoder left the card for the denoise phase: parked, packed, still quantized.
            self.assertEqual(self._tiers("enc"), {Tier.HOST})
        self.assertFalse(session.degraded)
        # RAM KEEP: between runs both components rest in RAM (packed host buffers).
        for name in ("enc", "dit"):
            self.assertEqual(self._tiers(name), {Tier.HOST}, name)
            component = self._handle().components[name]
            self.assertEqual(set(component.host_buffers), set(component.units))
            self.assert_forward(component.module, self.toys[name], f"{name} parked between runs")
        self._assert_all_rest("between runs")
        loaded = dict(self._handle().components)
        # A second run restores the parked components (no re-read) with identical output.
        with self._acquire() as again:
            self._run(again, "second run")
        for name in ("enc", "dit"):
            self.assertIs(self._handle().components[name], loaded[name], f"{name} was rebuilt instead of restored")
            self.assertEqual(self._tiers(name), {Tier.HOST}, name)
        self._assert_all_rest("after the second run")


if __name__ == "__main__":
    unittest.main()
