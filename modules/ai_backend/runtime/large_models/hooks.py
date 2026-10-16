"""
File: modules/ai_backend/runtime/large_models/hooks.py

Purpose:
Per-block residency hooks: run a component whose non-resident blocks live in
host memory (HOST tier) or only in its files (DISK tier) on the device, one
block at a time, with the next block copied in on a side stream while the
current one computes.

Main responsibilities:
- `apply_block_residency(component, transport, ...)` -> `HookSet`: forward
  pre/post hooks on every HOST / DISK block, `prefetch_depth + 1` device slots
  of the largest block, and for DISK blocks the same number of pinned host
  staging buffers plus one reader thread; the staging buffers are host
  allocations, so the live host guard (`HostWatch`) is checked before each;
- `HookSet.remove()` (also the context-manager exit): waits for every copy in
  flight, puts the blocks back in their rest state, frees the slots;
- `HookStats` (visits, prefetch hits, misses).

Contracts (measured defaults, see the package MODULE_README):
- A hooked block's weights are in the module only between its pre- and
  post-hook, as views into a device slot; at rest the module holds the parked
  host tensors (HOST) or `meta` placeholders (DISK). A block is "in flight"
  in the ledger (a staged transient DEVICE copy) for as long as a slot holds
  its bytes, from the fetch until the slot is reassigned or the set removed:
  so at most `prefetch_depth + 1` blocks are in flight, and the ledger's
  `max_in_flight` must allow that.
- Ordering (the event protocol): pre-hook of block i waits on the event its
  copy recorded (`wait(MAIN, copied)`); the post-hook records `used` on the
  main stream and only THEN issues the prefetch of the predicted next block,
  whose copy first waits on the `used` event of the slot it overwrites
  (`wait(SIDE, used)`). The prefetch is never issued before block i's compute
  is queued: a pageable copy blocks the host thread, so prefetching first hides
  nothing.
- A HOST block is parked as one packed pageable buffer with the slot's
  layout (`loader.packed_layout`), so it moves with ONE side-stream copy.
  DISK blocks are read by the reader thread with one positional read
  per contiguous span (file order, packed into a staging buffer at 256-byte
  aligned run starts), then ONE host-to-device copy per block; a staging
  buffer is reused only after its copy event completed. The same path serves
  every weight format. `ComponentSpec.convert` is applied to the device views.
- The next block is predicted from the observed forward order (first pass:
  the inventory order); a wrong prediction costs one synchronous fetch.
- One forward at a time per component (services serialize runs); hooked
  blocks must not nest.
- No host OOM: with a `watch`, `watch.check(n x staging bytes)` runs before
  the n-th staging buffer is allocated (the buffers already allocated are
  counted again: pageable pages may not be resident yet, so the fresh reading
  need not show them). `HostMemoryShort` frees every slot and staging buffer
  allocated so far, puts the blocks back in their rest state and propagates
  (the manager's host rung).

Notes:
- torch is imported lazily. Reads use `os.preadv` where the OS has it
  (Linux, macOS), else seek + readinto on a private unbuffered handle.
"""

from __future__ import annotations

import logging
import os
import queue
import threading
from collections.abc import Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from ..memory_budget import HostWatch
from ..safetensors_stream import read_safetensors_header_and_data_start
from .ledger import UnitRef, debug_enabled
from .loader import PACK_ALIGN, LoadedComponent, Stream, TensorSlot, Transport, packed_layout, packed_view
from .tiers import ComponentPlan, Tier

log = logging.getLogger(__name__)

#: The measured default: one block in flight beside the one computing.
DEFAULT_PREFETCH_DEPTH = 1

#: How long a waiting hook sleeps between checks that the reader is alive (s).
_READER_POLL_S = 0.5


class BlockStreamError(RuntimeError):
    """A DISK-tier block could not be read or copied; the forward that needed it fails."""


def _align(value: int) -> int:
    return (value + PACK_ALIGN - 1) // PACK_ALIGN * PACK_ALIGN


@dataclass(frozen=True)
class _DiskRun:
    """One positional read: `length` bytes at absolute `file_offset` into staging at `packed`."""

    path: Path
    file_offset: int
    length: int
    packed: int


@dataclass(frozen=True)
class _DiskTensor:
    """One file tensor inside the packed block bytes."""

    file_key: str
    packed: int
    nbytes: int
    dtype: Any
    shape: tuple[int, ...]


@dataclass
class _Block:
    """A hooked block: its unit, entries, rest tensors and slot layout."""

    ref: UnitRef
    tier: Tier
    entries: tuple[TensorSlot, ...]
    rest: tuple[Any, ...]
    host_buffer: Any = None
    host_offsets: tuple[int, ...] = ()
    disk_runs: tuple[_DiskRun, ...] = ()
    disk_tensors: tuple[_DiskTensor, ...] = ()
    nbytes: int = 0
    checked: bool = False


@dataclass
class _Slot:
    """One device slot; `holder` is the block whose bytes it holds (or is receiving)."""

    index: int
    buffer: Any
    holder: UnitRef | None = None
    ready: bool = False
    outstanding: int = 0
    copied: Any = None
    used: Any = None
    error: BaseException | None = None
    last_use: int = 0
    prefetched: bool = False


@dataclass
class HookStats:
    """Counters of one hook set: block visits, prefetch hits / misses, prefetches issued."""

    visits: int = 0
    hits: int = 0
    misses: int = 0
    prefetches: int = 0
    order: list[str] = field(default_factory=list)


def _disk_layout(spans: Sequence[Any], data_start: dict[Path, int]) -> tuple[tuple[_DiskRun, ...], tuple[_DiskTensor, ...], int]:
    """Pack a block's file tensors: contiguous spans of one file share one read, runs start aligned."""
    import torch

    runs: list[_DiskRun] = []
    tensors: list[_DiskTensor] = []
    packed = 0
    run_path: Path | None = None
    run_rel_start = run_rel_end = run_packed = 0
    ordered = sorted(spans, key=lambda entry: (str(entry.file), entry.span.start))
    for entry in ordered:
        dtype = getattr(torch, entry.span.torch_dtype_name)
        rel = entry.span.start - run_rel_start
        extends = run_path == entry.file and entry.span.start == run_rel_end and rel % dtype.itemsize == 0
        if not extends:
            if run_path is not None:
                runs.append(_DiskRun(run_path, data_start[run_path] + run_rel_start, run_rel_end - run_rel_start, run_packed))
                packed = run_packed + (run_rel_end - run_rel_start)
            run_packed = _align(packed)
            run_path, run_rel_start, run_rel_end = entry.file, entry.span.start, entry.span.start
            rel = 0
        tensors.append(_DiskTensor(entry.key, run_packed + rel, entry.span.nbytes, dtype, tuple(entry.span.shape)))
        run_rel_end = entry.span.end
    if run_path is not None:
        runs.append(_DiskRun(run_path, data_start[run_path] + run_rel_start, run_rel_end - run_rel_start, run_packed))
        packed = run_packed + (run_rel_end - run_rel_start)
    return tuple(runs), tuple(tensors), packed


def _view(buffer: Any, offset: int, nbytes: int, dtype: Any, shape: tuple[int, ...]) -> Any:
    return buffer[offset : offset + nbytes].view(dtype).view(shape)


class _Reader:
    """The DISK-tier reader thread: read a block into staging, copy it to its slot on the side stream."""

    def __init__(self, owner: HookSet, staging: Sequence[Any], paths: Sequence[Path]) -> None:
        self._owner = owner
        self._requests: queue.Queue[tuple[_Block, _Slot, Any] | None] = queue.Queue()
        self._free: queue.Queue[Any] = queue.Queue()
        for buffer in staging:
            self._free.put(buffer)
        self._paths = list(paths)
        self._handles: dict[Path, Any] = {}
        self._arrays: dict[int, Any] = {}
        self.thread = threading.Thread(target=self._run, name=f"large-model-reader:{owner.component.name}", daemon=True)
        self.thread.start()

    def request(self, block: _Block, slot: _Slot, wait_event: Any) -> None:
        self._requests.put((block, slot, wait_event))

    def stop(self) -> None:
        self._requests.put(None)
        self.thread.join()

    def _read_into(self, run: _DiskRun, array: Any) -> None:
        handle = self._handles.get(run.path)
        if handle is None:
            handle = run.path.open("rb", buffering=0)
            self._handles[run.path] = handle
        view = memoryview(array)[run.packed : run.packed + run.length]
        done = 0
        while done < run.length:
            if hasattr(os, "preadv"):
                got = os.preadv(handle.fileno(), [view[done:]], run.file_offset + done)
            else:
                handle.seek(run.file_offset + done)
                got = handle.readinto(view[done:])
            if not got:
                raise BlockStreamError(f"Файл {run.path} оборвался: прочитано {done} B из {run.length} B с позиции {run.file_offset}")
            done += got

    def _run(self) -> None:
        transport = self._owner.transport
        try:
            while True:
                item = self._requests.get()
                if item is None:
                    return
                block, slot, wait_event = item
                staging = self._free.get()
                try:
                    array = self._arrays.get(id(staging))
                    if array is None:
                        array = staging.numpy()
                        self._arrays[id(staging)] = array
                    for run in block.disk_runs:
                        self._read_into(run, array)
                    transport.wait(Stream.SIDE, wait_event)
                    transport.copy(slot.buffer[: block.nbytes], staging[: block.nbytes], Stream.SIDE)
                    event = transport.record(Stream.SIDE)
                    self._owner._post(slot, event, None)
                    # The staging buffer is rewritten only after its copy is done.
                    transport.synchronize(event)
                except (OSError, BlockStreamError, RuntimeError, ValueError) as exc:
                    log.error("large models: reading block %s of %s failed: %s", block.ref.label, self._owner.component.name, exc)
                    self._owner._post(slot, None, exc)
                finally:
                    self._free.put(staging)
        finally:
            for handle in self._handles.values():
                handle.close()
            self._handles.clear()


class HookSet:
    """Live residency hooks of one component (see the module contract). Use `remove()` or `with`."""

    def __init__(self, component: LoadedComponent, transport: Transport, *, prefetch_depth: int, plan: ComponentPlan | None = None, watch: HostWatch | None = None) -> None:
        if prefetch_depth < 0:
            raise ValueError(f"prefetch_depth должно быть >= 0, получено {prefetch_depth}")
        if component.hooks_active:
            raise RuntimeError(f"На компоненте «{component.name}» уже установлены хуки резидентности")
        if component.ledger.max_in_flight < prefetch_depth + 1:
            raise ValueError(f"Ledger компонента «{component.name}» допускает {component.ledger.max_in_flight} блок(ов) в пути, а prefetch_depth={prefetch_depth} требует {prefetch_depth + 1}")
        self.component = component
        self.transport = transport
        self.prefetch_depth = prefetch_depth
        self._watch = watch
        self.stats = HookStats()
        self._cond = threading.Condition()
        self._blocks: dict[UnitRef, _Block] = {}
        self._order: list[UnitRef] = []
        self._succ: dict[UnitRef, UnitRef] = {}
        self._slots: list[_Slot] = []
        self._staging: list[Any] = []
        self._handles: list[Any] = []
        self._reader: _Reader | None = None
        self._current: tuple[_Block, _Slot] | None = None
        self._last: UnitRef | None = None
        self._clock = 0
        self._removed = False
        self._owner_key = f"hooks:{component.name}"
        self._debug = debug_enabled()
        self._build(plan)

    # ------------------------------------------------------------- set-up
    def _build(self, plan: ComponentPlan | None) -> None:
        import torch

        component = self.component
        ledger = component.ledger
        not_resident = [ref.label for ref in component.units if ref.block is None and ledger.tier(ref) is not Tier.DEVICE]
        if not_resident:
            raise ValueError(f"Хуки резидентности: невыделенные веса «{component.name}» должны быть на устройстве ({not_resident[:3]})")
        blocks = [ref for ref in component.units if ref.block is not None]
        dropped = [ref.label for ref in blocks if ledger.tier(ref) is Tier.DROPPED]
        if dropped:
            raise ValueError(f"Хуки резидентности: блоки {dropped[:3]} выгружены (DROPPED); их нужно загрузить или пометить DISK")
        if plan is not None and plan.resident_blocks is not None:
            expected = [ref for index, ref in enumerate(blocks) if index < plan.resident_blocks]
            actual = [ref for ref in blocks if ledger.tier(ref) is Tier.DEVICE]
            if expected != actual:
                raise ValueError(f"Хуки резидентности: на устройстве блоки {[r.label for r in actual][:3]}…, а план требует первые {plan.resident_blocks}")
        hooked = [ref for ref in blocks if ledger.tier(ref) in (Tier.HOST, Tier.DISK)]
        if not hooked:
            log.debug("large models: %s has no HOST/DISK block, no hooks installed", component.name)
            return
        data_start: dict[Path, int] = {}
        for ref in hooked:
            if ledger.tier(ref) is Tier.DISK:
                for entry in component.unit_spans[ref]:
                    if entry.file not in data_start:
                        data_start[entry.file] = read_safetensors_header_and_data_start(entry.file)[1]
        slot_bytes = staging_bytes = 0
        for ref in hooked:
            entries = component.units[ref]
            tier = ledger.tier(ref)
            if tier is Tier.HOST:
                rest = tuple(entry.get() for entry in entries)
                buffer = component.host_buffers.get(ref)
                if buffer is None or any(tensor.device.type != "cpu" for tensor in rest):
                    raise ValueError(f"Хуки резидентности: блок {ref.label} в HOST, но не лежит одним упакованным буфером в оперативной памяти")
                offsets, nbytes = packed_layout(entries)
                block = _Block(ref, tier, entries, rest, host_buffer=buffer, host_offsets=offsets, nbytes=nbytes)
            else:
                rest = tuple(torch.empty(entry.shape, dtype=entry.dtype, device="meta") for entry in entries)
                runs, tensors, nbytes = _disk_layout(component.unit_spans[ref], data_start)
                block = _Block(ref, tier, entries, rest, disk_runs=runs, disk_tensors=tensors, nbytes=nbytes)
                staging_bytes = max(staging_bytes, nbytes)
                for entry, placeholder in zip(entries, rest):
                    entry.set(placeholder)
            slot_bytes = max(slot_bytes, block.nbytes)
            self._blocks[ref] = block
            self._order.append(ref)
        # First-pass prediction: the residency (inventory) order, cyclic.
        for current, following in zip(self._order, self._order[1:] + self._order[:1]):
            self._succ[current] = following
        count = self.prefetch_depth + 1
        try:
            self._slots = [_Slot(index, self.transport.alloc_device(slot_bytes)) for index in range(count)]
            pinned_all = True
            if staging_bytes:
                for index in range(count):
                    if self._watch is not None:
                        # The live host guard: the admission's reservation is a
                        # promise, not protection against memory taken since.
                        self._watch.check((index + 1) * staging_bytes)
                    buffer, pinned = self.transport.alloc_staging(staging_bytes)
                    pinned_all = pinned_all and pinned
                    self._staging.append(buffer)
        except BaseException:
            self._free_buffers()
            self._restore_rest_all()
            raise
        ledger.set_staging(self._owner_key, device_bytes=count * slot_bytes, host_bytes=len(self._staging) * staging_bytes)
        if self._staging:
            paths = sorted(data_start)
            self._reader = _Reader(self, self._staging, paths)
        for ref in hooked:
            submodule = component.module.get_submodule(ref.block or "")
            block = self._blocks[ref]
            self._handles.append(submodule.register_forward_pre_hook(self._make_pre(block)))
            self._handles.append(submodule.register_forward_hook(self._make_post(block)))
        component.hooks_active = True
        log.info(
            "large models: residency hooks on %s: %d block(s) (%d HOST, %d DISK), %d slot(s) x %.1f MiB on %s, staging %s x %.1f MiB%s",
            component.name, len(hooked), sum(1 for b in self._blocks.values() if b.tier is Tier.HOST), sum(1 for b in self._blocks.values() if b.tier is Tier.DISK),
            count, slot_bytes / 2**20, component.device, len(self._staging), staging_bytes / 2**20, "" if not self._staging or pinned_all else " (pageable fallback)",
        )

    # ------------------------------------------------------------- hooks
    def _make_pre(self, block: _Block) -> Any:
        def pre(module: Any, args: Any) -> None:
            self._pre(block)

        return pre

    def _make_post(self, block: _Block) -> Any:
        def post(module: Any, args: Any, output: Any) -> None:
            self._post_hook(block)

        return post

    def _pre(self, block: _Block) -> None:
        if self._current is not None:
            raise RuntimeError(f"Хуки резидентности: блок {block.ref.label} вызван внутри {self._current[0].ref.label} (вложенные блоки не поддерживаются)")
        if self._last is not None and self._last != block.ref:
            self._succ[self._last] = block.ref
        self.stats.visits += 1
        self.stats.order.append(block.ref.block or "")
        slot = self._slot_holding(block.ref)
        if slot is None:
            self.stats.misses += 1
            slot = self._pick_slot(exclude=())
            self._fetch(block, slot, prefetch=False)
        elif slot.prefetched:
            self.stats.hits += 1
        self._await_ready(slot, block)
        self._clock += 1
        slot.last_use = self._clock
        self._attach(block, slot)
        self._current = (block, slot)

    def _post_hook(self, block: _Block) -> None:
        current = self._current
        if current is None or current[0] is not block:
            raise RuntimeError(f"Хуки резидентности: пост-хук блока {block.ref.label} без парного пре-хука")
        _, slot = current
        # Recorded after the block's kernels were queued: the slot may be
        # overwritten only once they are done.
        slot.used = self.transport.record(Stream.MAIN)
        self._detach(block)
        self._current = None
        self._last = block.ref
        if self._debug:
            self.component.ledger.assert_single_copy()
        self._prefetch_after(block, slot)

    # ------------------------------------------------------------- slots
    def _slot_holding(self, ref: UnitRef) -> _Slot | None:
        return next((slot for slot in self._slots if slot.holder == ref), None)

    def _pick_slot(self, *, exclude: Sequence[_Slot]) -> _Slot | None:
        candidates = [slot for slot in self._slots if all(slot is not other for other in exclude)]
        if not candidates:
            return None
        return min(candidates, key=lambda slot: (slot.holder is not None, slot.last_use))

    def _prefetch_after(self, block: _Block, slot: _Slot) -> None:
        reserved: list[_Slot] = [slot]
        ref = block.ref
        for _ in range(self.prefetch_depth):
            following = self._succ.get(ref)
            if following is None or following == block.ref:
                return
            target = self._slot_holding(following)
            if target is None:
                target = self._pick_slot(exclude=reserved)
                if target is None:
                    return
                self._fetch(self._blocks[following], target, prefetch=True)
            reserved.append(target)
            ref = following

    def _fetch(self, block: _Block, slot: _Slot, *, prefetch: bool) -> None:
        with self._cond:
            # A slot still receiving an earlier DISK read is not reassigned:
            # the late side-stream copy would overwrite the new holder.
            while slot.outstanding:
                self._wait_locked()
            self._assign(slot, block.ref)
            slot.ready = False
            slot.copied = None
            slot.error = None
            slot.prefetched = prefetch
            wait_event = slot.used
        if prefetch:
            self.stats.prefetches += 1
        if block.tier is Tier.HOST:
            # The parked buffer has the slot's layout: one copy per block.
            self.transport.wait(Stream.SIDE, wait_event)
            self.transport.copy(slot.buffer[: block.nbytes], block.host_buffer[: block.nbytes], Stream.SIDE)
            event = self.transport.record(Stream.SIDE)
            with self._cond:
                slot.copied = event
                slot.ready = True
            return
        assert self._reader is not None  # a DISK block exists, so the reader was started
        with self._cond:
            slot.outstanding += 1
        self._reader.request(block, slot, wait_event)

    def _assign(self, slot: _Slot, ref: UnitRef | None) -> None:
        """Make `ref` the slot's holder: the old holder's slot copy is gone, the new one is in flight."""
        ledger = self.component.ledger
        if slot.holder is not None:
            ledger.release(slot.holder)
        slot.holder = None
        if ref is not None:
            ledger.begin_copy(ref, Tier.DEVICE, staged=True)
            slot.holder = ref

    def _post(self, slot: _Slot, event: Any, error: BaseException | None) -> None:
        """Reader-thread completion of a DISK fetch into `slot`."""
        with self._cond:
            slot.outstanding -= 1
            slot.copied = event
            slot.error = error
            slot.ready = error is None
            self._cond.notify_all()

    def _wait_locked(self) -> None:
        self._cond.wait(_READER_POLL_S)
        if self._reader is not None and not self._reader.thread.is_alive():
            raise BlockStreamError(f"Поток чтения блоков «{self.component.name}» остановился")

    def _await_ready(self, slot: _Slot, block: _Block) -> None:
        with self._cond:
            while not slot.ready and slot.error is None:
                self._wait_locked()
            if slot.error is not None:
                error = slot.error
                self._assign(slot, None)
                raise BlockStreamError(f"Не удалось подкачать блок {block.ref.label}: {error}") from error
            event = slot.copied
        self.transport.wait(Stream.MAIN, event)

    def _attach(self, block: _Block, slot: _Slot) -> None:
        if block.tier is Tier.HOST:
            for entry, offset in zip(block.entries, block.host_offsets):
                entry.set(packed_view(slot.buffer, offset, entry))
            return
        by_key = {entry.state_key: entry for entry in block.entries}
        convert = self.component.spec.convert
        for tensor in block.disk_tensors:
            view = _view(slot.buffer, tensor.packed, tensor.nbytes, tensor.dtype, tensor.shape)
            pairs = list(convert(tensor.file_key, view)) if convert is not None else [(key, view) for key in self.component.spec.state_keys_of(tensor.file_key)]
            for state_key, value in pairs:
                entry = by_key[state_key]
                if not block.checked and (tuple(value.shape) != entry.shape or value.dtype != entry.dtype):
                    raise ValueError(f"Хуки резидентности: «{state_key}» из файла имеет {tuple(value.shape)}/{value.dtype}, модель ждёт {entry.shape}/{entry.dtype}")
                entry.set(value)
        block.checked = True

    def _detach(self, block: _Block) -> None:
        for entry, rest in zip(block.entries, block.rest):
            entry.set(rest)

    # ------------------------------------------------------------- teardown
    def _restore_rest_all(self) -> None:
        for block in self._blocks.values():
            self._detach(block)

    def _free_buffers(self) -> None:
        for slot in self._slots:
            self.transport.free(slot.buffer)
        for buffer in self._staging:
            self.transport.free(buffer)
        self._slots = []
        self._staging = []

    def remove(self) -> None:
        """Wait for every copy in flight, put every hooked block in its rest state, free slots and staging. Idempotent."""
        if self._removed:
            return
        self._removed = True
        for handle in self._handles:
            handle.remove()
        self._handles.clear()
        if self._current is not None:
            # A forward failed inside a hooked block: its post-hook never ran.
            block, _ = self._current
            self._detach(block)
            self._current = None
        if self._reader is not None:
            self._reader.stop()
            self._reader = None
        for slot in self._slots:
            self.transport.synchronize(slot.copied)
            self.transport.synchronize(slot.used)
            self._assign(slot, None)
        self._restore_rest_all()
        self._free_buffers()
        # Drop every reference to the parked tensors: a removed hook set that
        # is still referenced (stats read afterwards) must not keep a RAM copy
        # alive once `moves.restore` brought the blocks back to the device.
        self._blocks.clear()
        self._succ.clear()
        self.component.ledger.clear_staging(self._owner_key)
        self.component.hooks_active = False
        log.debug("large models: residency hooks of %s removed (visits %d, hits %d, misses %d)", self.component.name, self.stats.visits, self.stats.hits, self.stats.misses)

    def __enter__(self) -> HookSet:
        return self

    def __exit__(self, *exc_info: object) -> None:
        self.remove()


def apply_block_residency(component: LoadedComponent, transport: Transport, *, prefetch_depth: int = DEFAULT_PREFETCH_DEPTH, plan: ComponentPlan | None = None, watch: HostWatch | None = None) -> HookSet:
    """Install residency hooks on every HOST / DISK block of `component` (tiers read from its ledger).

    `plan` (optional) is checked against the ledger: a partial plan must find
    exactly its first `resident_blocks` blocks on the device. `watch` (the
    session's live host guard) is checked before every DISK staging buffer.
    A component with no HOST / DISK block gets an empty hook set.

    # Raises
    `ValueError` when non-block weights are not on the device, a block is
    DROPPED, a HOST block's tensors are not in host memory, or the plan does
    not match; `RuntimeError` when hooks are already active;
    `HostMemoryShort` from `watch` before a staging buffer; whatever the
    transport raises allocating slots or staging (in every failure case the
    buffers allocated so far are freed and the blocks are back in their rest
    state).
    """
    return HookSet(component, transport, prefetch_depth=prefetch_depth, plan=plan, watch=watch)
