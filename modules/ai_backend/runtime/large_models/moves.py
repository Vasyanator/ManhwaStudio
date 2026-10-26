"""
File: modules/ai_backend/runtime/large_models/moves.py

Purpose:
The phase-transition choreography of a large model: move residency units
between the device, the host and the disk ONE UNIT AT A TIME, interleaving
what leaves the device with what comes in, so neither the device nor the host
ever holds both states at once.

Main responsibilities:
- `transfer_block_wise(outgoing, incoming, ...)` -> `TransferReport`;
- `park` (every device unit of a component to HOST / DISK / DROPPED),
  `restore` (every off-device unit back), `drop` (forget everything);
- `DeviceShort` (the transfer cannot fit `device_limit`).

Contracts:
- Device peak <= max(start, end) + one unit, measured on the allocator's
  RESERVED bytes (`Transport.device_reserved`; the ledger's device bytes on a
  transport that cannot measure): an incoming unit is admitted only while it
  fits under `max(start, end)` (and `device_limit`), otherwise one outgoing
  unit is evicted first. `Transport.release_cache()` (`empty_cache`) runs
  after every evicted unit and at the end of the transfer: without it the
  reserved peak reaches the SUM of both states whenever incoming allocations
  differ in size from the freed blocks (measured), and the forecast assumes
  the cache is released at every phase boundary.
- Host peak <= max(start, end) + one unit: a parked unit is copied into ONE
  fresh packed pageable buffer (`Transport.alloc_host`, `loader.packed_layout`)
  and its device copy freed before the next unit moves; a restored unit frees
  its packed buffer right after the device copies exist.
- `HostMemoryShort` from `watch` before a unit is parked switches that unit
  and every REMAINING outgoing unit from HOST to DROPPED (logged); the host is
  never allocated past the guard, and the dropped units are re-read from disk
  when next needed. An incoming unit that cannot be read propagates its error
  (the transfer stops at a consistent point: every unit fully moved or not at
  all). On a host run device (`LoadedComponent.device_is_host`) an incoming
  unit's copy is host memory too: `watch` is checked before it, and its
  `HostMemoryShort` propagates like any incoming failure.
- Every tier change goes through the ledger (`begin_copy` / `commit`,
  `move_without_copy`) and `assert_single_copy()` runs after every unit; with
  `MS_LARGE_MODELS_DEBUG=1` and a torch transport the ledger is also
  cross-checked against `memory_allocated` and `RssAnon + RssShmem`.
- Sources are only the mechanics' own anonymous host tensors, never a
  file-mapped tensor (the ROCm amdkfd stall); the debug mode asserts it.
- A component with live residency hooks is refused (`RuntimeError`).
- Disk re-reads report progress only when the caller passes `read_progress`:
  one `LoadProgress` stream (`reread=True`) per component over ALL of its
  DISK / DROPPED incoming units of the transfer, rate-limited once here
  (`ByteProgress`, the filler itself reports every chunk to it), ending with
  one frame at 100% after that component's last unit was read.

Notes:
- torch is imported lazily through the loader / transport.
"""

from __future__ import annotations

import logging
import time
from collections.abc import Sequence
from dataclasses import dataclass, field
from typing import Any

from ..memory_budget import HostMemoryShort, HostWatch
from ..safetensors_stream import ByteProgress, ByteProgressCb
from .ledger import Ledger, UnitRef, debug_enabled
from .loader import LoadedComponent, LoadProgressSink, TorchLedgerProbe, TorchTransport, Transport, component_read_progress, load_units, packed_layout, packed_view, set_unit_to_meta
from .tiers import Tier

log = logging.getLogger(__name__)

#: Cross-check tolerances of the debug mode (bytes).
_DEBUG_DEVICE_TOLERANCE = 64 * 1024**2
_DEBUG_HOST_SLACK = 256 * 1024**2


class DeviceShort(RuntimeError):
    """The transfer's end state does not fit `device_limit`; nothing was moved."""

    def __init__(self, needed: int, limit: int) -> None:
        super().__init__(f"device short: the transfer ends at {needed} B, limit {limit} B")
        self.needed = needed
        self.limit = limit


@dataclass
class TransferReport:
    """What one transfer did.

    `evicted` / `admitted` in execution order; `dropped_instead` the outgoing
    units that were asked to go to HOST and were dropped because the host
    guard fired; `device_start` / `device_end` / `device_peak` the measured
    device figure (reserved bytes, or ledger bytes) before, after and at its
    highest; `seconds` wall time.
    """

    evicted: list[UnitRef] = field(default_factory=list)
    admitted: list[UnitRef] = field(default_factory=list)
    dropped_instead: list[UnitRef] = field(default_factory=list)
    device_start: int = 0
    device_end: int = 0
    device_peak: int = 0
    seconds: float = 0.0

    @property
    def host_short(self) -> bool:
        """Whether the host guard fired during the transfer."""
        return bool(self.dropped_instead)


Move = tuple[LoadedComponent, UnitRef]


def _ledgers(moves: Sequence[Move]) -> list[Ledger]:
    seen: list[Ledger] = []
    for component, _ in moves:
        if all(component.ledger is not ledger for ledger in seen):
            seen.append(component.ledger)
    return seen


class _RereadProgress:
    """The disk re-reads of one transfer, one rate-limited `LoadProgress` stream per component.

    Built before anything moves from the incoming units still in DISK /
    DROPPED: a component's total is the file bytes of exactly those units
    (what `load_units` will read for them).
    """

    def __init__(self, incoming: Sequence[Move], sink: LoadProgressSink, owner: str) -> None:
        totals: dict[LoadedComponent, int] = {}
        for component, ref in incoming:
            if component.ledger.tier(ref) in (Tier.DISK, Tier.DROPPED):
                totals[component] = totals.get(component, 0) + sum(span.file_bytes for span in component.unit_spans[ref])
        self._streams = {component: ByteProgress(component_read_progress(component.inventory, sink, reread=True), total, owner=owner, logger=log) for component, total in totals.items()}
        self._totals = totals

    def callback_for(self, component: LoadedComponent) -> ByteProgressCb | None:
        """The per-`load_units` callback feeding `component`'s stream with byte deltas; `None` when it re-reads nothing."""
        stream = self._streams.get(component)
        if stream is None:
            return None
        total = self._totals[component]
        last = 0

        def report(done: int, _unit_total: int, key: str) -> None:
            nonlocal last
            delta = done - last
            last = done
            stream.advance(delta, key, final=stream.done_bytes + delta >= total)

        return report


class _Mover:
    """State of one `transfer_block_wise` call."""

    def __init__(self, moves: Sequence[Move], transport: Transport, watch: HostWatch | None, owner: str, reread: _RereadProgress | None = None) -> None:
        self.transport = transport
        self.watch = watch
        self.owner = owner
        self.reread = reread
        self.ledgers = _ledgers(moves)
        self.debug = debug_enabled()
        if self.debug and isinstance(transport, TorchTransport):
            probe = TorchLedgerProbe(transport)
            for ledger in self.ledgers:
                ledger.arm_cross_check(probe)

    def measure(self) -> int:
        reserved = self.transport.device_reserved()
        if reserved is not None:
            return reserved
        return sum(ledger.holdings().device_bytes for ledger in self.ledgers)

    def after_unit(self, component: LoadedComponent, ref: UnitRef) -> None:
        component.ledger.assert_single_copy()
        if self.debug:
            nbytes = component.unit_bytes[ref]
            component.ledger.cross_check(device_tolerance=_DEBUG_DEVICE_TOLERANCE + nbytes, host_slack=_DEBUG_HOST_SLACK + nbytes, context=f"{self.owner}: {ref.label}")

    def evict(self, component: LoadedComponent, ref: UnitRef, to: Tier) -> None:
        """Move one DEVICE unit to `to` (HOST: copy then free; DISK/DROPPED: free)."""
        ledger = component.ledger
        entries = component.units[ref]
        if to is Tier.HOST:
            ledger.begin_copy(ref, Tier.HOST)
            offsets, nbytes = packed_layout(entries)
            buffer = None
            try:
                buffer = self.transport.alloc_host(nbytes)
                views = [packed_view(buffer, offset, entry) for entry, offset in zip(entries, offsets)]
                for entry, view in zip(entries, views):
                    view.copy_(entry.get())
            except BaseException:
                if buffer is not None:
                    self.transport.free(buffer)
                ledger.release(ref)
                raise
            for entry, view in zip(entries, views):
                device_tensor = entry.get()
                entry.set(view)
                self.transport.free(device_tensor)
                del device_tensor
            component.host_buffers[ref] = buffer
            ledger.commit(ref)
        else:
            for entry in entries:
                device_tensor = entry.get()
                self.transport.free(device_tensor)
                del device_tensor
            set_unit_to_meta(component, ref)
            ledger.move_without_copy(ref, to)
        # Released after every evicted unit: incoming allocations of another
        # size cannot reuse the freed blocks, and reserved memory would
        # otherwise grow to the sum of both states.
        self.transport.release_cache()
        self.after_unit(component, ref)

    def admit(self, component: LoadedComponent, ref: UnitRef) -> None:
        """Bring one HOST / DISK / DROPPED unit onto the device."""
        ledger = component.ledger
        tier = ledger.tier(ref)
        if tier is Tier.HOST:
            entries = component.units[ref]
            if self.debug:
                self._assert_anonymous(component, ref)
            if component.device_is_host and self.watch is not None:
                # The "device" copy is host memory: guard it before it exists.
                self.watch.check(component.unit_bytes[ref])
            ledger.begin_copy(ref, Tier.DEVICE)
            moved: list[tuple[Any, Any]] = []
            try:
                for entry in entries:
                    host = entry.get()
                    moved.append((host, self.transport.to_device(host)))
            except BaseException:
                for _, device_tensor in moved:
                    self.transport.free(device_tensor)
                ledger.release(ref)
                raise
            for entry, (host, device_tensor) in zip(entries, moved):
                entry.set(device_tensor)
            del moved
            self._free_host(component, ref)
            ledger.commit(ref)
        else:
            progress = None if self.reread is None else self.reread.callback_for(component)
            # Interval 0: the per-component stream above throttles once.
            load_units(component, [ref], watch=self.watch, progress=progress, progress_interval_seconds=0.0)
            for entry in component.units[ref]:
                self.transport.track_device(entry.get())
        self.after_unit(component, ref)

    def _free_host(self, component: LoadedComponent, ref: UnitRef) -> None:
        """Release the packed host buffer of a unit whose entries no longer view it."""
        buffer = component.host_buffers.pop(ref, None)
        if buffer is not None:
            self.transport.free(buffer)

    @staticmethod
    def _assert_anonymous(component: LoadedComponent, ref: UnitRef) -> None:
        from ..rocm_mmap_transfer import tensor_needs_staging

        first = component.units[ref][0].get()
        if tensor_needs_staging(first):
            raise RuntimeError(f"large models: {ref.label} лежит в файловом отображении; такой тензор нельзя переносить на ROCm GPU")


def transfer_block_wise(
    outgoing: Sequence[Move],
    incoming: Sequence[Move],
    *,
    outgoing_to: Tier,
    transport: Transport,
    watch: HostWatch | None = None,
    device_limit: int | None = None,
    owner: str = "large models",
    read_progress: LoadProgressSink | None = None,
) -> TransferReport:
    """Move `outgoing` DEVICE units to `outgoing_to` and bring `incoming` units onto the device, interleaved.

    `outgoing` / `incoming` are `(component, unit)` pairs, each list in the
    order it is executed; `outgoing_to` is `HOST`, `DISK` or `DROPPED`;
    `incoming` units may be in `HOST`, `DISK` or `DROPPED` (the latter two are
    re-read from their files). `device_limit` (bytes, same measure as
    `Transport.device_reserved`) is the ceiling the device may never exceed.
    `read_progress` receives the disk re-reads (module contracts), on the
    calling thread; a HOST -> device restore reads no file and reports nothing.

    # Raises
    `ValueError` for a unit in the wrong tier or an invalid `outgoing_to`;
    `RuntimeError` for a component with live hooks; `DeviceShort` (before
    anything moves) when the end state exceeds `device_limit`; a device OOM,
    a host allocation failure or a read error of an incoming unit propagate
    after the unit in progress was rolled back.
    """
    if outgoing_to not in (Tier.HOST, Tier.DISK, Tier.DROPPED):
        raise ValueError(f"transfer_block_wise: outgoing_to должен быть HOST/DISK/DROPPED, получено {outgoing_to}")
    for component, ref in [*outgoing, *incoming]:
        if component.hooks_active:
            raise RuntimeError(f"transfer_block_wise: на компоненте «{component.name}» активны хуки резидентности")
        if ref not in component.units:
            raise ValueError(f"transfer_block_wise: у «{component.name}» нет блока {ref.label}")
    wrong_out = [ref.label for component, ref in outgoing if component.ledger.tier(ref) is not Tier.DEVICE]
    wrong_in = [ref.label for component, ref in incoming if component.ledger.tier(ref) is Tier.DEVICE]
    if wrong_out or wrong_in:
        raise ValueError(f"transfer_block_wise: уходящие блоки не на устройстве {wrong_out[:3]}, приходящие уже на устройстве {wrong_in[:3]}")

    started = time.monotonic()
    reread = None if read_progress is None else _RereadProgress(incoming, read_progress, owner)
    mover = _Mover([*outgoing, *incoming], transport, watch, owner, reread)
    report = TransferReport()
    start = mover.measure()
    out_bytes = sum(component.unit_bytes[ref] for component, ref in outgoing)
    in_bytes = sum(component.unit_bytes[ref] for component, ref in incoming)
    end = max(0, start - out_bytes) + in_bytes
    if device_limit is not None and end > device_limit:
        raise DeviceShort(end, device_limit)
    ceiling = max(start, end) if device_limit is None else min(max(start, end), max(device_limit, end))
    report.device_start = report.device_peak = start
    pending_out = list(outgoing)
    pending_in = list(incoming)
    target = outgoing_to

    def evict_next() -> None:
        nonlocal target
        component, ref = pending_out.pop(0)
        to = target
        if to is Tier.HOST and mover.watch is not None:
            try:
                mover.watch.check(component.unit_bytes[ref])
            except HostMemoryShort as exc:
                log.warning("%s: host memory short before parking %s (need %d B, headroom %d B); this and the remaining %d outgoing unit(s) are dropped instead and will be re-read from disk", owner, ref.label, exc.needed, exc.headroom, len(pending_out))
                target = to = Tier.DROPPED
        mover.evict(component, ref, to)
        report.evicted.append(ref)
        if outgoing_to is Tier.HOST and to is Tier.DROPPED:
            report.dropped_instead.append(ref)

    while pending_in:
        component, ref = pending_in[0]
        nbytes = component.unit_bytes[ref]
        current = mover.measure()
        if pending_out and current + nbytes > ceiling:
            evict_next()
            continue
        mover.admit(component, ref)
        pending_in.pop(0)
        report.admitted.append(ref)
        report.device_peak = max(report.device_peak, mover.measure())
    while pending_out:
        evict_next()
    transport.release_cache()
    report.device_end = mover.measure()
    report.device_peak = max(report.device_peak, report.device_end)
    report.seconds = time.monotonic() - started
    log.info(
        "%s: block-wise transfer: %d unit(s) out to %s (%d dropped instead), %d in; device %.2f -> %.2f GiB, peak %.2f GiB; %.2f s",
        owner, len(report.evicted), outgoing_to.value, len(report.dropped_instead), len(report.admitted),
        report.device_start / 1024**3, report.device_end / 1024**3, report.device_peak / 1024**3, report.seconds,
    )
    return report


def park(component: LoadedComponent, *, to: Tier = Tier.HOST, transport: Transport, watch: HostWatch | None = None, owner: str = "large models") -> TransferReport:
    """Move every DEVICE unit of `component` to `to` (HOST, DISK or DROPPED), block by block."""
    outgoing = [(component, ref) for ref in component.refs(tiers=(Tier.DEVICE,))]
    return transfer_block_wise(outgoing, [], outgoing_to=to, transport=transport, watch=watch, owner=owner)


def restore(component: LoadedComponent, *, transport: Transport, watch: HostWatch | None = None, device_limit: int | None = None, owner: str = "large models", read_progress: LoadProgressSink | None = None) -> TransferReport:
    """Bring every off-device unit of `component` back onto its device (non-block weights first); `read_progress` as in `transfer_block_wise`."""
    incoming = [(component, ref) for ref in component.refs(tiers=(Tier.HOST, Tier.DISK, Tier.DROPPED))]
    return transfer_block_wise([], incoming, outgoing_to=Tier.DROPPED, transport=transport, watch=watch, device_limit=device_limit, owner=owner, read_progress=read_progress)


def drop(component: LoadedComponent, *, transport: Transport) -> int:
    """Forget every copy of `component`'s weights (every unit -> DROPPED); returns the bytes released.

    # Raises
    `RuntimeError` for a component with live hooks.
    """
    if component.hooks_active:
        raise RuntimeError(f"drop: на компоненте «{component.name}» активны хуки резидентности")
    ledger = component.ledger
    released = 0
    for ref in component.refs():
        tier = ledger.tier(ref)
        if tier is Tier.DROPPED:
            continue
        if tier is Tier.DEVICE:
            for entry in component.units[ref]:
                transport.free(entry.get())
        if tier in (Tier.DEVICE, Tier.HOST):
            set_unit_to_meta(component, ref)
            buffer = component.host_buffers.pop(ref, None)
            if buffer is not None:
                transport.free(buffer)
            released += component.unit_bytes[ref]
        ledger.move_without_copy(ref, Tier.DROPPED)
    transport.release_cache()
    log.info("large models: %s dropped (%.2f GiB released)", component.name, released / 1024**3)
    return released
