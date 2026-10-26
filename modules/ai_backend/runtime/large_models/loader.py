"""
File: modules/ai_backend/runtime/large_models/loader.py

Purpose:
The torch-facing base of the mechanics layer: load one component of a large
model straight from its checkpoint files (safetensors or GGUF) into the tiers
a `ComponentPlan` names (device, host or left on `meta`), bind its residency
units to the module tensors they own, and the device-operations seam
(`Transport`) every move and residency hook goes through.

Main responsibilities:
- `load_component` - build the component on `meta` (`ComponentSpec.build_meta`)
  and fill it through `runtime.safetensors_stream.fill_module_from_safetensors`
  (THE one filler; it opens each file through `runtime.tensor_container`)
  with per-key `targets` derived from the plan; register every unit in the
  handle's `Ledger`; `HostWatch` checked before the first read and
  at every file tensor, load transient included;
- `load_units` - re-read some DISK/DROPPED units of a loaded component onto
  its device (a phase transition bringing a dropped block back);
- disk-read progress of a component (`LoadProgress`, `LoadProgressSink`,
  `component_read_progress`): what a session's load-progress sink receives;
- `LoadedComponent` with its unit binding (`TensorSlot`, `set_unit_to_meta`)
  and the packed layout of a unit (`packed_layout`, `packed_view`,
  `PACK_ALIGN`), shared by parked host buffers and device slots;
- `Transport` (the protocol) and `TorchTransport` (CUDA/HIP streams and
  events, or a synchronous CPU "device"), `Stream`, `TorchLedgerProbe`.

Contracts:
- Host memory: one file tensor at a time (the filler's contract): its read
  buffer plus the conversion outputs that do not alias it are alive at once
  (`BlockSpan.load_host_bytes`, the transient the forecast adds to a
  boundary that reads a component, `ComponentInventory.load_host_bytes`). A HOST
  unit lives in ONE packed pageable buffer (`torch.empty`, never pinned,
  never file-mapped), its entries are views into it: one H2D copy moves it to
  a slot, and a block buffer (>= 32 MiB) is always its own `mmap`, so freeing
  it returns the RAM at once (per-tensor allocations below glibc's mmap
  threshold stay in its arenas after `free`: measured 1.5 of 2.9 GiB still
  resident after a restore). Nothing here moves a file-mapped (`rw-p`) tensor
  to a ROCm GPU.
- Stored layout: every entry's shape / dtype is what the spec's conversion
  produces, derived from the headers by running the conversion on `meta`
  (so a DISK unit's placeholders already have the stored dtype).
- A failure mid-load (`HostMemoryShort`, a device OOM, a refusal of the
  filler) leaves the ledger untouched and the partial module is discarded;
  `load_units` resets the units it was filling to `meta` and leaves their
  ledger tier unchanged. Errors propagate.
- Non-block weights always travel with the component: on its run device when
  any part of it runs there, else on the host.
- `Transport.free(t)` is called for every device or host copy the mechanics
  discard, `Transport.track_device(t)` for every device tensor the filler
  allocated on their behalf. The torch transport needs nothing there
  (reference counting frees it); a fake transport keeps an exact byte ledger
  with them.

Notes:
- torch is imported lazily (inside functions / at `TorchTransport`
  construction) so importing this module stays torch-free.
- Whole-module moves are not done here; a caller that needs one uses
  `runtime.rocm_mmap_transfer.move_module_to`.
"""

from __future__ import annotations

import logging
import time
from collections.abc import Callable, Iterable, Iterator, Mapping, Sequence
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from types import MappingProxyType
from typing import Any, Protocol

from ..memory_budget import HostWatch
from ..safetensors_stream import (
    LEAVE_ON_META,
    PROGRESS_MIN_INTERVAL_SECONDS,
    SAFETENSORS_DTYPES_WITH_F8,
    ByteProgressCb,
    FillReport,
    fill_module_from_safetensors,
)
from .inventory import BlockSpan, ComponentInventory
from .ledger import Ledger, UnitRef, host_private_probe
from .spec import ComponentSpec, ExecDevice
from .tiers import ComponentPlan, Tier

log = logging.getLogger(__name__)


# =====================================================================
#  Device-operations seam
# =====================================================================
class Stream(Enum):
    """The two streams of the mechanics: the compute stream and the copy side stream."""

    MAIN = "main"
    SIDE = "side"


class Transport(Protocol):
    """Every byte the mechanics move goes through this seam (tests fake it).

    `device` is the run device. Copies issued on `Stream.SIDE` are ordered
    only by events (`record` / `wait`); on a synchronous transport they
    complete before returning and events are `None`.
    """

    device: Any

    def alloc_device(self, nbytes: int) -> Any:
        """A `uint8` device buffer of `nbytes` (a residency hook's slot)."""
        ...

    def alloc_staging(self, nbytes: int) -> tuple[Any, bool]:
        """A `uint8` host buffer of `nbytes`, pinned when possible; returns `(buffer, pinned)`."""
        ...

    def to_device(self, tensor: Any) -> Any:
        """A fresh device copy of host `tensor` (synchronous)."""
        ...

    def alloc_host(self, nbytes: int) -> Any:
        """A PAGEABLE `uint8` host buffer of `nbytes` (a parked unit's packed copy; never pinned)."""
        ...

    def free(self, tensor: Any) -> None:
        """The mechanics no longer reference `tensor` (accounting hook)."""
        ...

    def track_device(self, tensor: Any) -> None:
        """A device tensor allocated outside the transport (by the filler) is now held (accounting hook)."""
        ...

    def release_cache(self) -> None:
        """Return the allocator's cached free blocks to the device (`empty_cache`)."""
        ...


    def device_reserved(self) -> int | None:
        """Bytes the device allocator reserves now; `None` when not measurable."""
        ...

    def copy(self, dst: Any, src: Any, stream: Stream) -> None:
        """`dst.copy_(src)` issued on `stream` (asynchronous where the backend allows)."""
        ...

    def record(self, stream: Stream) -> Any:
        """Record an event on `stream` after everything issued so far; `None` on a synchronous transport."""
        ...

    def wait(self, stream: Stream, event: Any) -> None:
        """Make `stream` wait for `event` (no host block); `None` is a no-op."""
        ...

    def synchronize(self, event: Any) -> None:
        """Block the calling thread until `event` completed; `None` is a no-op."""
        ...


class TorchTransport:
    """`Transport` over real torch: CUDA/HIP streams and events, or a synchronous CPU device.

    On a CUDA/HIP device `Stream.MAIN` is the current stream at call time and
    `Stream.SIDE` a dedicated copy stream created here. Host staging is pinned
    by registering a plain allocation when the driver allows (fallback:
    pageable, logged once at WARNING); `free` unregisters it.
    """

    def __init__(self, device: Any) -> None:
        import torch

        self._torch = torch
        self.device = torch.device(device)
        self._cuda = self.device.type == "cuda"
        self._side = torch.cuda.Stream(self.device) if self._cuda else None
        self._pin_warned = False
        self._registered: set[int] = set()

    def _stream(self, stream: Stream) -> Any:
        if stream is Stream.SIDE:
            return self._side
        return self._torch.cuda.current_stream(self.device)

    def alloc_device(self, nbytes: int) -> Any:
        return self._torch.empty(int(nbytes), dtype=self._torch.uint8, device=self.device)

    def alloc_staging(self, nbytes: int) -> tuple[Any, bool]:
        # Pinned by REGISTERING a plain allocation (cudaHostRegister /
        # hipHostRegister), not through `pin_memory=True`: torch's caching
        # host allocator keeps freed pinned blocks (and frees a block that
        # served a non_blocking copy only after its events were processed),
        # so the staging stayed resident after the hook set was gone
        # (measured: +0.52 GiB RSS on gfx1201). A registered buffer is
        # unregistered in `free` and its pages go back with the allocation.
        torch = self._torch
        buffer = torch.empty(int(nbytes), dtype=torch.uint8)
        if not self._cuda:
            return buffer, False
        try:
            status = torch.cuda.cudart().cudaHostRegister(buffer.data_ptr(), int(nbytes), 0)
        except (AttributeError, RuntimeError) as exc:
            status = exc
        if getattr(status, "value", status) == 0:
            self._registered.add(buffer.data_ptr())
            return buffer, True
        # Pinning can fail (ulimit -l, the ROCm TTM cap); pageable staging is
        # ~1.4x slower on CUDA and equal on ROCm, never wrong.
        if not self._pin_warned:
            log.warning("large models: pinning a %d B staging buffer on %s failed (%s); using pageable staging", nbytes, self.device, status)
            self._pin_warned = True
        return buffer, False

    def to_device(self, tensor: Any) -> Any:
        out = self._torch.empty(tuple(tensor.shape), dtype=tensor.dtype, device=self.device)
        out.copy_(tensor)
        return out

    def alloc_host(self, nbytes: int) -> Any:
        return self._torch.empty(int(nbytes), dtype=self._torch.uint8)

    def free(self, tensor: Any) -> None:
        # Reference counting frees the storage once the module and the
        # mechanics drop it; only a registered staging buffer needs its
        # registration undone first (no copy may still read it: the hook set
        # synchronizes every copy event before freeing).
        pointer = tensor.data_ptr()
        if pointer in self._registered:
            self._registered.discard(pointer)
            status = self._torch.cuda.cudart().cudaHostUnregister(pointer)
            if getattr(status, "value", status) != 0:
                log.warning("large models: unregistering a staging buffer on %s failed (%s)", self.device, status)

    def track_device(self, tensor: Any) -> None:
        return None

    def release_cache(self) -> None:
        if self._cuda:
            self._torch.cuda.empty_cache()

    def device_reserved(self) -> int | None:
        if self._cuda:
            return int(self._torch.cuda.memory_reserved(self.device))
        return None

    def device_allocated(self) -> int | None:
        """`torch.cuda.memory_allocated` of the run device; `None` on CPU."""
        if self._cuda:
            return int(self._torch.cuda.memory_allocated(self.device))
        return None

    def copy(self, dst: Any, src: Any, stream: Stream) -> None:
        if not self._cuda:
            dst.copy_(src)
            return
        with self._torch.cuda.stream(self._stream(stream)):
            dst.copy_(src, non_blocking=True)

    def record(self, stream: Stream) -> Any:
        if not self._cuda:
            return None
        event = self._torch.cuda.Event()
        event.record(self._stream(stream))
        return event

    def wait(self, stream: Stream, event: Any) -> None:
        if event is None or not self._cuda:
            return
        self._stream(stream).wait_event(event)

    def synchronize(self, event: Any) -> None:
        if event is not None:
            event.synchronize()


class TorchLedgerProbe:
    """`LedgerProbe` over a `TorchTransport` (device allocated) and `memory_budget` (RssAnon + RssShmem)."""

    def __init__(self, transport: TorchTransport) -> None:
        self._transport = transport
        self._host = host_private_probe()

    def device_allocated(self) -> int | None:
        return self._transport.device_allocated()

    def host_private(self) -> int | None:
        return self._host()


# =====================================================================
#  Unit binding
# =====================================================================
#: Alignment of every entry inside a packed unit buffer or a device slot (bytes).
PACK_ALIGN = 256


@dataclass(frozen=True)
class TensorSlot:
    """One state entry of a module: the owning submodule, its attribute name, parameter or buffer.

    `shape` / `dtype` are the entry's STORED layout (what the spec's
    conversion produces, derived from the headers on `meta`): what a `meta`
    placeholder, a packed host view and a slot view reproduce.
    """

    state_key: str
    owner: Any
    leaf: str
    is_param: bool
    shape: tuple[int, ...]
    dtype: Any

    def get(self) -> Any:
        """The tensor currently stored in the module."""
        return self.owner._parameters[self.leaf] if self.is_param else self.owner._buffers[self.leaf]

    def set(self, tensor: Any) -> None:
        """Store `tensor` (a parameter becomes `nn.Parameter(tensor, requires_grad=False)`)."""
        if self.is_param:
            import torch

            self.owner._parameters[self.leaf] = torch.nn.Parameter(tensor, requires_grad=False)
        else:
            self.owner._buffers[self.leaf] = tensor

    @property
    def nbytes(self) -> int:
        """Bytes of one copy of this entry."""
        elements = 1
        for dim in self.shape:
            elements *= dim
        return elements * self.dtype.itemsize


def packed_layout(entries: Sequence[TensorSlot]) -> tuple[tuple[int, ...], int]:
    """Offsets of `entries` packed back to back at `PACK_ALIGN` and the total size (bytes).

    The one layout of a unit's packed host buffer AND of a device slot, so a
    parked unit moves to a slot with one copy.
    """
    offsets: list[int] = []
    cursor = 0
    for entry in entries:
        cursor = (cursor + PACK_ALIGN - 1) // PACK_ALIGN * PACK_ALIGN
        offsets.append(cursor)
        cursor += entry.nbytes
    return tuple(offsets), cursor


def packed_view(buffer: Any, offset: int, entry: TensorSlot) -> Any:
    """`entry`'s tensor view into a `uint8` buffer at `offset`."""
    return buffer[offset : offset + entry.nbytes].view(entry.dtype).view(entry.shape)


def unit_of_span(component: str, span: BlockSpan) -> UnitRef:
    """The residency unit a file tensor belongs to: its block, or itself when it is a non-block weight."""
    return UnitRef(component, block=span.block) if span.block is not None else UnitRef(component, tensor=span.key)


def inventory_units(inventory: ComponentInventory) -> dict[UnitRef, tuple[BlockSpan, ...]]:
    """Every unit of `inventory` with its file tensors: non-block tensors first, then blocks in residency order."""
    units: dict[UnitRef, tuple[BlockSpan, ...]] = {}
    for span in inventory.non_block:
        units[unit_of_span(inventory.name, span)] = (span,)
    for name, spans in inventory.blocks.items():
        units[UnitRef(inventory.name, block=name)] = spans
    return units


# =====================================================================
#  Disk-read progress of one component
# =====================================================================
@dataclass(frozen=True)
class LoadProgress:
    """One disk-read progress frame of one component of a large model.

    `component` is the `ComponentSpec.name`; `file` the checkpoint file the
    bytes were last read from (`None` only for the empty frame of a read that
    reads nothing); `files` every file of the component, in read order (a
    single-file component or a sharded folder: the caller picks the label);
    `done_bytes` / `total_bytes` count file bytes of THIS read only: a whole
    component for a fresh load (`reread=False`), only the DISK / DROPPED units
    a transition brings back for a re-read (`reread=True`). `done_bytes` never
    decreases within one read and its last frame has `done_bytes ==
    total_bytes`.
    """

    component: str
    file: Path | None
    files: tuple[Path, ...]
    done_bytes: int
    total_bytes: int
    reread: bool


#: A session's load-progress sink (`Session.set_load_progress`): called on the
#: thread that drives the session, synchronously inside the read loop.
LoadProgressSink = Callable[[LoadProgress], None]


def component_read_progress(inventory: ComponentInventory, sink: LoadProgressSink, *, reread: bool) -> ByteProgressCb:
    """A filler `ByteProgressCb` that turns `(done, total, last_key)` into `LoadProgress` frames of `inventory`'s component.

    The file of a frame is the one that holds `last_key` (the tensor being
    read). Rate limiting stays with the filler's `ByteProgress`; this adds none.
    """
    file_of_key = {span.key: span.file for spans in inventory_units(inventory).values() for span in spans}
    files = tuple(inventory.files)

    def report(done: int, total: int, last_key: str) -> None:
        sink(LoadProgress(component=inventory.name, file=file_of_key.get(last_key), files=files, done_bytes=done, total_bytes=total, reread=reread))

    return report


@dataclass(eq=False)
class LoadedComponent:
    """One component of a large model, loaded into a module, with its units bound.

    `units` maps each unit to the module entries it owns, `unit_spans` to its
    file tensors, `unit_bytes` to its device bytes (the ledger figure), all in
    `inventory_units` order. `host_buffers` holds the packed pageable buffer
    of every HOST unit (its entries are views into it). `device` is the run
    device; `device_is_host` says its memory IS the host's (`cpu` / `mps`): a
    DEVICE unit then costs host memory, so the live host guard counts it
    (loads here, restores in `moves`). `hooks_active` is set by a live
    residency hook set (moves refuse to touch such a component).
    """

    spec: ComponentSpec
    inventory: ComponentInventory
    module: Any
    device: Any
    ledger: Ledger
    options: Any
    weight_format: str
    units: Mapping[UnitRef, tuple[TensorSlot, ...]]
    unit_spans: Mapping[UnitRef, tuple[BlockSpan, ...]]
    unit_bytes: Mapping[UnitRef, int]
    report: FillReport | None = None
    host_buffers: dict[UnitRef, Any] = field(default_factory=dict)
    hooks_active: bool = field(default=False)
    device_is_host: bool = field(default=False)

    @property
    def name(self) -> str:
        """The component name."""
        return self.spec.name

    def refs(self, *, tiers: Iterable[Tier] | None = None) -> list[UnitRef]:
        """Units in `inventory_units` order, optionally only those in one of `tiers`."""
        wanted = None if tiers is None else set(tiers)
        return [ref for ref in self.units if wanted is None or self.ledger.tier(ref) in wanted]

    def block_ref(self, block: str) -> UnitRef:
        """The unit of block `block`; `KeyError` when the component has no such block."""
        ref = UnitRef(self.name, block=block)
        if ref not in self.units:
            raise KeyError(block)
        return ref


def set_unit_to_meta(component: LoadedComponent, ref: UnitRef) -> None:
    """Replace every entry of `ref` with a `meta` placeholder of its stored layout (its bytes are released)."""
    _set_meta(component.units[ref])


def _set_meta(entries: Sequence[TensorSlot]) -> None:
    import torch

    for entry in entries:
        entry.set(torch.empty(entry.shape, dtype=entry.dtype, device="meta"))


def _inner_convert(spec: ComponentSpec) -> Any:
    if spec.convert is not None:
        return spec.convert

    def identity(file_key: str, tensor: Any) -> Iterator[tuple[str, Any]]:
        for state_key in spec.state_keys_of(file_key):
            yield state_key, tensor

    return identity


def _bind_units(spec: ComponentSpec, module: Any, units: Mapping[UnitRef, tuple[BlockSpan, ...]]) -> dict[UnitRef, tuple[TensorSlot, ...]]:
    """Bind every unit's state entries, with the stored layout derived by running the conversion on `meta`.

    # Raises
    `ValueError` when a block is not a submodule path, a state key is not a
    parameter / buffer of `module`, or the conversion does not yield the
    declared keys (it must be device-agnostic: it runs on `meta` here).
    """
    import torch

    convert = _inner_convert(spec)
    bound: dict[UnitRef, tuple[TensorSlot, ...]] = {}
    for ref, spans in units.items():
        slots: list[TensorSlot] = []
        for span in spans:
            source = torch.empty(span.span.shape, dtype=getattr(torch, span.span.torch_dtype_name), device="meta")
            produced = list(convert(span.key, source))
            if sorted(key for key, _ in produced) != sorted(span.state_keys):
                raise ValueError(f"Компонент «{spec.name}»: преобразование «{span.key}» на meta дало {sorted(k for k, _ in produced)}, ожидались {sorted(span.state_keys)}")
            for state_key, value in produced:
                owner_path, _, leaf = state_key.rpartition(".")
                owner = module.get_submodule(owner_path) if owner_path else module
                if leaf in owner._parameters:
                    is_param = True
                elif leaf in owner._buffers:
                    is_param = False
                else:
                    raise ValueError(f"Компонент «{spec.name}»: в модуле нет параметра или буфера «{state_key}»")
                slots.append(TensorSlot(state_key, owner, leaf, is_param, tuple(value.shape), value.dtype))
        if ref.block is not None:
            prefix = ref.block + "."
            outside = [slot.state_key for slot in slots if not slot.state_key.startswith(prefix)]
            if outside:
                raise ValueError(f"Компонент «{spec.name}»: блок «{ref.block}» должен быть путём подмодуля, а ключи {outside[:3]} лежат вне него")
        bound[ref] = tuple(slots)
    return bound


# =====================================================================
#  Loading
# =====================================================================
def initial_tiers(plan: ComponentPlan, inventory: ComponentInventory) -> dict[UnitRef, Tier]:
    """The tier of every unit right after loading `inventory` per `plan`.

    Whole-component plans put every unit in `plan.tier`; a partial plan
    (`resident_blocks = k`) puts the non-block weights and the first `k`
    blocks on the device and the rest in `plan.tier` (`HOST` or `DISK`).
    """
    tiers: dict[UnitRef, Tier] = {}
    resident = plan.resident_blocks
    position = {name: index for index, name in enumerate(inventory.block_names)}
    for ref in inventory_units(inventory):
        if resident is None:
            tiers[ref] = plan.tier
        elif ref.block is None:
            tiers[ref] = Tier.DEVICE
        else:
            tiers[ref] = Tier.DEVICE if position[ref.block] < resident else plan.tier
    return tiers


def _require_loadable(spec: ComponentSpec, inventory: ComponentInventory) -> None:
    if spec.build_meta is None:
        raise ValueError(f"Компонент «{spec.name}»: в спецификации нет build_meta, загрузить его нельзя")
    if spec.convert is None:
        multi = [span.key for span in [*inventory.non_block, *(s for spans in inventory.blocks.values() for s in spans)] if len(span.state_keys) != 1]
        if multi:
            raise ValueError(f"Компонент «{spec.name}»: тензоры {multi[:3]} дают несколько ключей, нужен ComponentSpec.convert")


def _move_non_persistent_buffers(module: Any, device: Any) -> None:
    """Put every non-persistent buffer (not in `state_dict`, so never filled) on `device`."""
    persistent = set(module.state_dict().keys())
    for name, buffer in list(module.named_buffers()):
        if name in persistent or buffer.device == device:
            continue
        owner_path, _, leaf = name.rpartition(".")
        owner = module.get_submodule(owner_path) if owner_path else module
        owner._buffers[leaf] = buffer.to(device)


def _fill(
    component_name: str,
    spec: ComponentSpec,
    inventory: ComponentInventory,
    module: Any,
    bound: Mapping[UnitRef, tuple[TensorSlot, ...]],
    *,
    device: Any,
    tiers: Mapping[UnitRef, Tier],
    wanted: frozenset[UnitRef] | None,
    host_buffers: dict[UnitRef, Any],
    watch: HostWatch | None,
    progress: ByteProgressCb | None,
    owner: str,
    device_is_host: bool = False,
    progress_interval_seconds: float = PROGRESS_MIN_INTERVAL_SECONDS,
) -> FillReport:
    """One filler call writing every read entry into its unit's place.

    `wanted` = the units to read (`None` = every unit not in DISK / DROPPED);
    the others stay on `meta`. DEVICE-tier units are copied to `device`;
    HOST-tier units are copied into one packed pageable buffer per unit
    (`host_buffers`, allocated at the unit's first tensor). `watch` is checked
    before the first read with the largest load transient of the tensors this
    call reads, and at every file tensor (its read buffer already allocated)
    with the bytes that will stay in RAM (DEVICE-tier reads too when
    `device_is_host`: the run device's memory is the host's) plus what is still
    to be allocated before the next check: this tensor's conversion outputs, or
    the largest transient of the reads still to come less this tensor's read
    buffer (freed by then). The guard therefore never asks for more than the
    forecast's `host weights + load transient`. `progress` /
    `progress_interval_seconds` go to the filler unchanged.
    """
    import torch

    unit_of_key: dict[str, UnitRef] = {}
    slot_of_key: dict[str, tuple[TensorSlot, int]] = {}
    layouts: dict[UnitRef, int] = {}
    for ref, entries in bound.items():
        offsets, nbytes = packed_layout(entries)
        layouts[ref] = nbytes
        for entry, offset in zip(entries, offsets):
            unit_of_key[entry.state_key] = ref
            slot_of_key[entry.state_key] = (entry, offset)

    def reads(ref: UnitRef) -> bool:
        if wanted is not None:
            return ref in wanted
        return tiers[ref] in (Tier.DEVICE, Tier.HOST)

    inner = _inner_convert(spec)
    span_of_file_key = {span.key: span for spans in inventory_units(inventory).values() for span in spans}
    # The filler's read order: files in order, each in file order, a span read
    # when its unit is (every state key of a span belongs to its one unit).
    read_order = [span for path in inventory.files for span in sorted((s for s in span_of_file_key.values() if s.file == path), key=lambda s: s.span.start) if reads(unit_of_span(component_name, span))]
    position = {span.key: index for index, span in enumerate(read_order)}
    # suffix[i] = the largest load transient of the reads from the i-th on.
    suffix = [0] * (len(read_order) + 1)
    for index in range(len(read_order) - 1, -1, -1):
        suffix[index] = max(suffix[index + 1], read_order[index].load_host_bytes)
    if watch is not None and read_order:
        watch.check(suffix[0])

    def convert(file_key: str, tensor: Any) -> Iterable[tuple[str, Any]]:
        # The live host guard at every file tensor (module notes, `_fill`).
        if watch is not None:
            span = span_of_file_key.get(file_key)
            if span is not None and file_key in position:
                ref = unit_of_span(component_name, span)
                stays = span.device_bytes if tiers[ref] is Tier.HOST or device_is_host else 0
                upcoming = suffix[position[file_key] + 1] - span.file_bytes
                watch.check(stays + max(span.convert_bytes, upcoming))
        return list(inner(file_key, tensor))

    def targets(state_key: str) -> Any:
        ref = unit_of_key.get(state_key)
        if ref is None:
            # A key the inventory does not know: the filler refuses it as unexpected.
            return device
        return device if reads(ref) else LEAVE_ON_META

    def write(writes: Sequence[Any]) -> None:
        for state_key, value, place in writes:
            entry, offset = slot_of_key[state_key]
            if tuple(value.shape) != entry.shape or value.dtype != entry.dtype:
                raise ValueError(f"{owner}: вес «{state_key}» имеет {tuple(value.shape)}/{value.dtype}, а ожидается {entry.shape}/{entry.dtype}")
            ref = unit_of_key[state_key]
            if tiers[ref] is Tier.HOST and wanted is None:
                buffer = host_buffers.get(ref)
                if buffer is None:
                    buffer = torch.empty(layouts[ref], dtype=torch.uint8)
                    host_buffers[ref] = buffer
                view = packed_view(buffer, offset, entry)
                view.copy_(value)
                entry.set(view)
            else:
                entry.set(value.to(place))

    return fill_module_from_safetensors(
        module,
        list(inventory.files),
        device=device,
        convert=convert,
        expected_keys_of=spec.state_keys_of,
        targets=targets,
        write=write,
        dtypes=SAFETENSORS_DTYPES_WITH_F8,
        progress=progress,
        progress_interval_seconds=progress_interval_seconds,
        log_summary=False,
        owner=owner,
        logger=log,
    )


def load_component(
    spec: ComponentSpec,
    inventory: ComponentInventory,
    plan: ComponentPlan,
    *,
    options: Any,
    weight_format: str,
    device: Any,
    ledger: Ledger,
    watch: HostWatch | None = None,
    progress: ByteProgressCb | None = None,
    owner: str | None = None,
    device_is_host: bool = False,
) -> LoadedComponent:
    """Build `spec`'s module on `meta` and fill it per `plan` from `inventory.files`.

    DEVICE units land on `device`; HOST units are copied, one file tensor at a
    time, into one packed pageable buffer per unit; DISK / DROPPED units are
    not read and hold `meta` placeholders of their stored layout. With
    `device_is_host` (a `cpu` / `mps` run device) the watch also counts the
    DEVICE units' bytes, and the component remembers it
    (`LoadedComponent.device_is_host`). Every unit is
    registered in `ledger` with its tier only after the fill succeeded; the
    module is put in `eval()` mode.

    # Raises
    `ValueError` for a spec without `build_meta`, a 1:N tensor without
    `convert`, a conversion that does not run on `meta`, a block name that is
    not a submodule path, or any refusal of the filler; `HostMemoryShort` from
    `watch`; a device OOM as torch raises it. On any failure no unit is
    registered and the module is discarded.
    """
    import torch

    _require_loadable(spec, inventory)
    label = owner or f"large model component «{spec.name}»"
    started = time.monotonic()
    target = torch.device(device)
    tiers = initial_tiers(plan, inventory)
    host = torch.device("cpu")
    assert spec.build_meta is not None  # checked by _require_loadable
    module = spec.build_meta(options, weight_format)
    host_buffers: dict[UnitRef, Any] = {}
    units = inventory_units(inventory)
    try:
        bound = _bind_units(spec, module, units)
        report = _fill(spec.name, spec, inventory, module, bound, device=target, tiers=tiers, wanted=None, host_buffers=host_buffers, watch=watch, progress=progress, owner=label, device_is_host=device_is_host)
        for ref, tier in tiers.items():
            if tier in (Tier.DISK, Tier.DROPPED):
                _set_meta(bound[ref])
        # Non-persistent buffers are not ledger units (small, never in a
        # checkpoint): they live on the device the component executes on,
        # whatever tier its weights are in, so a parked component restored by
        # `moves` needs nothing else.
        _move_non_persistent_buffers(module, target if plan.exec_device is ExecDevice.RUN_DEVICE else host)
        module.eval()
    except BaseException:
        log.error("%s: loading %s (format %s, plan tier %s, resident blocks %s) failed; the partial module is discarded", label, spec.name, weight_format, plan.tier.value, plan.resident_blocks)
        host_buffers.clear()
        del module
        raise
    unit_bytes = {ref: sum(span.device_bytes for span in spans) for ref, spans in units.items()}
    for ref, tier in tiers.items():
        ledger.register(ref, unit_bytes[ref], tier)
    counts = {tier: sum(1 for value in tiers.values() if value is tier) for tier in Tier}
    log.info(
        "%s: loaded %s to %s in %.1f s: %d units on device, %d in RAM, %d on disk/dropped; %.2f GiB read",
        label, spec.name, target, time.monotonic() - started, counts[Tier.DEVICE], counts[Tier.HOST], counts[Tier.DISK] + counts[Tier.DROPPED], report.bytes_read / 1024**3,
    )
    return LoadedComponent(
        spec=spec,
        inventory=inventory,
        module=module,
        device=target,
        ledger=ledger,
        options=options,
        weight_format=weight_format,
        units=MappingProxyType(bound),
        unit_spans=MappingProxyType(units),
        unit_bytes=MappingProxyType(unit_bytes),
        report=report,
        host_buffers=host_buffers,
        device_is_host=device_is_host,
    )


def load_units(component: LoadedComponent, refs: Sequence[UnitRef], *, watch: HostWatch | None = None, progress: ByteProgressCb | None = None, progress_interval_seconds: float = PROGRESS_MIN_INTERVAL_SECONDS) -> FillReport:
    """Read the DISK / DROPPED units `refs` of `component` from its files onto its device (watched as in `load_component`).

    `progress` gets the filler's byte frames of these units only, at most every
    `progress_interval_seconds` plus the final one (`0.0`: every read chunk,
    for a caller that aggregates several calls and throttles itself).

    At most `ledger.max_in_flight` units per call (each is in flight until the
    fill finished); a block-wise move passes one. Each unit's ledger entry
    goes `begin_copy(DEVICE)` -> `commit` once the fill succeeded. On failure
    the units are reset to `meta`, their ledger entries released (tier
    unchanged) and the error propagates.

    # Raises
    `ValueError` when a unit is not in DISK / DROPPED or is unknown; whatever
    the filler, `watch` or the device raise.
    """
    import torch

    ledger = component.ledger
    for ref in refs:
        if ref not in component.units:
            raise ValueError(f"load_units: у компонента «{component.name}» нет блока {ref.label}")
        if ledger.tier(ref) not in (Tier.DISK, Tier.DROPPED):
            raise ValueError(f"load_units: блок {ref.label} в {ledger.tier(ref).value}, а не на диске")
    tiers = {ref: ledger.tier(ref) for ref in component.units}
    started_refs: list[UnitRef] = []
    try:
        for ref in refs:
            ledger.begin_copy(ref, Tier.DEVICE)
            started_refs.append(ref)
        report = _fill(component.name, component.spec, component.inventory, component.module, component.units, device=torch.device(component.device), tiers=tiers, wanted=frozenset(refs), host_buffers=component.host_buffers, watch=watch, progress=progress, owner=f"large model component «{component.name}»", device_is_host=component.device_is_host, progress_interval_seconds=progress_interval_seconds)
    except BaseException:
        log.error("large model component «%s»: re-reading %d unit(s) from disk failed; they stay on disk", component.name, len(refs))
        for ref in started_refs:
            set_unit_to_meta(component, ref)
            ledger.release(ref)
        raise
    for ref in refs:
        ledger.commit(ref)
    return report
