"""
File: modules/ai_backend/runtime/large_models/_mechanics_test_fixtures.py

Purpose:
Shared fixtures of the mechanics tests (`test_loader`, `test_hooks`,
`test_moves`): a real-torch toy model with residency blocks, a safetensors
writer with real data and a controllable layout, the toy's `ComponentSpec`,
and fake transports that keep an exact byte ledger of "device" and "host"
memory on the CPU.

Key declarations:
- `ToyNet` / `ToyBlock` - embed -> N blocks (two linears, a persistent `gain`
  buffer, a non-persistent `scratch` buffer) -> head; `forward(x, order=None)`
  runs the blocks in `order`; every block forward is appended to `COMPUTE_LOG`
  with its weight's data pointer;
- `write_safetensors(path, tensors, ...)` and `write_toy_checkpoint(...)`;
- `toy_component_spec(path)`, `toy_inventory(spec)`;
- `FakeTransport` (synchronous, byte pools, call log) and `DeferredTransport`
  (side-stream copies run only when an event that covers them is waited on).

Notes:
Test-only; files go to caller-owned temporary directories. Requires torch.
"""

from __future__ import annotations

import json
import struct
import threading
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any

import torch
from torch import nn

from ..memory_budget import HostBudget, HostWatch
from .inventory import ComponentInventory, build_inventory
from .loader import LoadedComponent, Stream
from .spec import ComponentSpec, Role
from .tiers import Tier

#: Every `ToyBlock.forward` appends `("compute", block_index, fc1.weight.data_ptr())`.
COMPUTE_LOG: list[tuple[str, int, int]] = []

_DTYPE_TOKEN = {torch.float32: "F32", torch.bfloat16: "BF16", torch.uint8: "U8", torch.float16: "F16"}


class ToyBlock(nn.Module):
    """One residency block: `x + fc2(gelu(fc1(x))) * gain` (+ `flag` folded in, a uint8 buffer)."""

    def __init__(self, index: int, width: int = 16, hidden: int = 32) -> None:
        super().__init__()
        self.index = index
        self.fc1 = nn.Linear(width, hidden)
        self.fc2 = nn.Linear(hidden, width)
        self.register_buffer("gain", torch.ones(width))
        self.register_buffer("flag", torch.zeros(3, dtype=torch.uint8))
        self.register_buffer("scratch", torch.ones(1), persistent=False)

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        COMPUTE_LOG.append(("compute", self.index, self.fc1.weight.data_ptr()))
        bump = self.flag.to(x.dtype).sum() * 0.01
        return x + self.fc2(torch.nn.functional.gelu(self.fc1(x))) * self.gain * self.scratch + bump


class ToyNet(nn.Module):
    """embed -> `blocks` -> head; `forward(x, order)` runs the blocks in `order` (default: index order)."""

    def __init__(self, blocks: int = 6, width: int = 16) -> None:
        super().__init__()
        self.embed = nn.Linear(8, width)
        self.blocks = nn.ModuleList([ToyBlock(index, width) for index in range(blocks)])
        self.head = nn.Linear(width, 4)

    def forward(self, x: torch.Tensor, order: Sequence[int] | None = None) -> torch.Tensor:
        h = self.embed(x)
        for index in order if order is not None else range(len(self.blocks)):
            h = self.blocks[index](h)
        return self.head(h)


def build_toy_meta(blocks: int = 6) -> ToyNet:
    """A `ToyNet` on `meta` with its non-persistent buffers real (the loader's precondition)."""
    with torch.device("meta"):
        net = ToyNet(blocks)
    for block in net.blocks:
        block._buffers["scratch"] = torch.ones(1)
    return net


def reference_toy(blocks: int = 6, seed: int = 0) -> ToyNet:
    """A fully materialized `ToyNet` with seeded random weights and buffers."""
    generator = torch.Generator().manual_seed(seed)
    net = ToyNet(blocks)
    with torch.no_grad():
        for name, tensor in [*net.named_parameters(), *net.named_buffers()]:
            if name.endswith("scratch"):
                continue
            if tensor.dtype == torch.uint8:
                tensor.copy_(torch.randint(0, 5, tensor.shape, generator=generator, dtype=torch.uint8))
            else:
                tensor.copy_(torch.randn(tensor.shape, generator=generator))
    return net.eval()


def write_safetensors(path: Path, tensors: Sequence[tuple[str, torch.Tensor]], *, align: int = 1) -> Path:
    """Write `tensors` in the given order (data back to back, each start rounded up to `align`)."""
    header: dict[str, Any] = {}
    chunks: list[bytes] = []
    offset = 0
    for name, tensor in tensors:
        data = tensor.detach().contiguous().cpu().view(torch.uint8).numpy().tobytes() if tensor.dtype != torch.uint8 else tensor.detach().contiguous().numpy().tobytes()
        padding = (-offset) % align
        if padding:
            chunks.append(b"\0" * padding)
            offset += padding
        header[name] = {"dtype": _DTYPE_TOKEN[tensor.dtype], "shape": list(tensor.shape), "data_offsets": [offset, offset + len(data)]}
        chunks.append(data)
        offset += len(data)
    payload = json.dumps(header).encode("utf-8")
    path.write_bytes(struct.pack("<Q", len(payload)) + payload + b"".join(chunks))
    return path


def toy_state(net: ToyNet) -> dict[str, torch.Tensor]:
    """The persistent state of `net` (what a checkpoint holds)."""
    return {key: value.detach().clone() for key, value in net.state_dict().items()}


def write_toy_checkpoint(path: Path, net: ToyNet, *, interleave: bool = False) -> Path:
    """Save `net`'s state; `interleave=True` alternates tensors of neighbouring blocks (fragmented blocks)."""
    items = list(toy_state(net).items())
    if interleave:
        blocks = [item for item in items if item[0].startswith("blocks.")]
        others = [item for item in items if not item[0].startswith("blocks.")]
        evens = blocks[::2]
        odds = blocks[1::2]
        items = others[:2] + [entry for pair in zip(evens, odds) for entry in pair] + others[2:]
    return write_safetensors(path, items)


def toy_block_of(key: str) -> str | None:
    """`blocks.<i>.*` -> `blocks.<i>`; everything else is a non-block weight."""
    parts = key.split(".")
    return f"blocks.{parts[1]}" if parts[0] == "blocks" else None


def toy_component_spec(path: Path, *, blocks: int = 6, name: str = "net", block_of: Any = toy_block_of, **fields: Any) -> ComponentSpec:
    """The toy component: one file, blocks `blocks.<i>`, built on meta by `build_toy_meta`."""
    values: dict[str, Any] = dict(build_meta=lambda options, weight_format: build_toy_meta(blocks))
    values.update(fields)
    return ComponentSpec(name, Role.DIT, lambda options, weight_format: [path], block_of, **values)


def toy_inventory(spec: ComponentSpec) -> ComponentInventory:
    """`build_inventory` of the toy spec's file."""
    return build_inventory(spec, spec.files(None, "f32"))


class _FakeEvent:
    _ids = 0

    def __init__(self, stream: Stream, upto: int) -> None:
        _FakeEvent._ids += 1
        self.id = _FakeEvent._ids
        self.stream = stream
        self.upto = upto


class FakeTransport:
    """A synchronous CPU transport with exact "device" / "host" byte pools and a call log.

    Pools are keyed by `data_ptr`; `peak_device` / `peak_host` are their
    highest sums. `log` records `("copy", stream, dst_ptr, src_ptr)`,
    `("record", stream, event_id)`, `("wait", stream, event_id)`.
    """

    def __init__(self) -> None:
        self.device = torch.device("cpu")
        self.device_pool: dict[int, int] = {}
        self.host_pool: dict[int, int] = {}
        self.peak_device = 0
        self.peak_host = 0
        self.log: list[tuple[Any, ...]] = []
        self.released = 0
        self.slots: list[torch.Tensor] = []
        self.lock = threading.Lock()

    @staticmethod
    def _nbytes(tensor: torch.Tensor) -> int:
        return tensor.numel() * tensor.element_size()

    def _add(self, pool: dict[int, int], tensor: torch.Tensor) -> None:
        with self.lock:
            pool[tensor.data_ptr()] = self._nbytes(tensor)
            self.peak_device = max(self.peak_device, sum(self.device_pool.values()))
            self.peak_host = max(self.peak_host, sum(self.host_pool.values()))

    def device_bytes(self) -> int:
        """Bytes in the device pool now."""
        return sum(self.device_pool.values())

    def host_bytes(self) -> int:
        """Bytes in the host pool now."""
        return sum(self.host_pool.values())

    def reset_peaks(self) -> None:
        """Peaks restart from the current pool sums."""
        self.peak_device = self.device_bytes()
        self.peak_host = self.host_bytes()

    def adopt(self, component: LoadedComponent) -> None:
        """Register the component's current tensors (DEVICE units) and packed host buffers (HOST units)."""
        for ref, entries in component.units.items():
            tier = component.ledger.tier(ref)
            if tier is Tier.DEVICE:
                for entry in entries:
                    self._add(self.device_pool, entry.get())
            elif tier is Tier.HOST:
                self._add(self.host_pool, component.host_buffers[ref])

    def alloc_device(self, nbytes: int) -> torch.Tensor:
        buffer = torch.zeros(int(nbytes), dtype=torch.uint8)
        self._add(self.device_pool, buffer)
        self.slots.append(buffer)
        return buffer

    def alloc_staging(self, nbytes: int) -> tuple[torch.Tensor, bool]:
        buffer = torch.zeros(int(nbytes), dtype=torch.uint8)
        self._add(self.host_pool, buffer)
        return buffer, True

    def to_device(self, tensor: torch.Tensor) -> torch.Tensor:
        out = tensor.detach().clone()
        self._add(self.device_pool, out)
        return out

    def alloc_host(self, nbytes: int) -> torch.Tensor:
        buffer = torch.empty(int(nbytes), dtype=torch.uint8)
        self._add(self.host_pool, buffer)
        return buffer

    def free(self, tensor: torch.Tensor) -> None:
        with self.lock:
            ptr = tensor.data_ptr()
            if ptr in self.device_pool:
                del self.device_pool[ptr]
            elif ptr in self.host_pool:
                del self.host_pool[ptr]

    def track_device(self, tensor: torch.Tensor) -> None:
        self._add(self.device_pool, tensor)

    def release_cache(self) -> None:
        self.released += 1

    def device_reserved(self) -> int | None:
        return self.device_bytes()

    def slot_of(self, ptr: int) -> int | None:
        """Index of the device slot that contains address `ptr`, if any."""
        for index, slot in enumerate(self.slots):
            start = slot.data_ptr()
            if start <= ptr < start + slot.numel():
                return index
        return None

    def copy(self, dst: torch.Tensor, src: torch.Tensor, stream: Stream) -> None:
        with self.lock:
            self.log.append(("copy", stream, dst.data_ptr(), src.data_ptr()))
        dst.copy_(src)

    def record(self, stream: Stream) -> Any:
        event = _FakeEvent(stream, 0)
        with self.lock:
            self.log.append(("record", stream, event.id))
        return event

    def wait(self, stream: Stream, event: Any) -> None:
        if event is not None:
            with self.lock:
                self.log.append(("wait", stream, event.id))

    def synchronize(self, event: Any) -> None:
        return None


class DeferredTransport(FakeTransport):
    """`FakeTransport` whose SIDE-stream copies are queued and run only when an event covering them is waited on.

    A consumer that reads a slot without `wait(MAIN, copied)` sees stale
    bytes, so a forward through hooks that skipped the event protocol gives a
    wrong result.
    """

    def __init__(self) -> None:
        super().__init__()
        self._queue: list[tuple[int, torch.Tensor, torch.Tensor]] = []
        self._seq = 0
        self._done = 0

    def copy(self, dst: torch.Tensor, src: torch.Tensor, stream: Stream) -> None:
        if stream is not Stream.SIDE:
            super().copy(dst, src, stream)
            return
        with self.lock:
            self.log.append(("copy", stream, dst.data_ptr(), src.data_ptr()))
            self._seq += 1
            # The source is snapshotted: a real async copy reads it at issue
            # time for pageable memory, and staging is reused only after sync.
            self._queue.append((self._seq, dst, src.clone()))

    def record(self, stream: Stream) -> Any:
        with self.lock:
            event = _FakeEvent(stream, self._seq if stream is Stream.SIDE else 0)
            self.log.append(("record", stream, event.id))
        return event

    def _flush(self, upto: int) -> None:
        """Run every queued SIDE copy with sequence <= `upto`; on return they are all done.

        The copies run under the lock: the reader thread's `synchronize` and
        the main thread's `wait(MAIN, copied)` flush concurrently, and a flush
        that dequeued the copies but had not run them yet would let the other
        caller return over stale slot bytes (the S-093 flake).
        """
        with self.lock:
            ready = [item for item in self._queue if item[0] <= upto]
            self._queue = [item for item in self._queue if item[0] > upto]
            for _, dst, src in ready:
                dst.copy_(src)

    def wait(self, stream: Stream, event: Any) -> None:
        super().wait(stream, event)
        if event is not None and stream is Stream.MAIN and event.stream is Stream.SIDE:
            self._flush(event.upto)

    def synchronize(self, event: Any) -> None:
        if event is not None and event.stream is Stream.SIDE:
            self._flush(event.upto)


def _budget(available: int) -> HostBudget:
    return HostBudget(total=available, available=available, cgroup_limit=None, cgroup_used=None, rss_anon=0, rss_shmem=0, reserve=0)


class ShortAfter:
    """A host budget that is ample for `calls` reads, then empty (`watch()` -> a real `HostWatch`)."""

    def __init__(self, calls: int) -> None:
        self.calls = calls
        self.count = 0

    def budget(self) -> HostBudget:
        self.count += 1
        return _budget(1 << 50 if self.count <= self.calls else 0)

    def watch(self) -> HostWatch:
        return HostWatch(self.budget, floor=1)


class Recording(HostWatch):
    """A `HostWatch` over an ample budget that records every `needed` it is asked for."""

    def __init__(self) -> None:
        super().__init__(lambda: _budget(1 << 50))
        self.needed: list[int] = []

    def check(self, needed: int = 0) -> HostBudget:
        self.needed.append(int(needed))
        return super().check(needed)

    def watch(self) -> HostWatch:
        return self


def same_state(module: nn.Module, reference: Mapping[str, torch.Tensor]) -> bool:
    """Whether every persistent entry of `module` equals `reference` exactly."""
    state = module.state_dict()
    return state.keys() == reference.keys() and all(torch.equal(state[key].cpu(), reference[key]) for key in reference)
