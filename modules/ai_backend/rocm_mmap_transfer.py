"""
File: modules/ai_backend/rocm_mmap_transfer.py

Purpose:
Work around a ROCm/amdkfd pathology that makes model loading pathologically
slow on AMD GPUs: a host->device copy whose source CPU tensor lives in a
*writable private file mapping* (`rw-p`, which is exactly how safetensors /
transformers / diffusers hand out weights after `from_pretrained`) stalls inside
the KFD driver for seconds, because registering such a range for DMA forces the
kernel to break copy-on-write over the resident pages. Materializing the tensor
in anonymous memory first (`Tensor.clone()`) removes the stall entirely.

Measured on this project's ROCm 7.2 host (see "Measurements" below for the full
matrix): Surya OCR, 1.34 GiB, 271.06 s -> 0.28 s.

Main responsibilities:
- detect a ROCm/HIP Torch build (same signal as `rocm_runtime.py`);
- detect whether a CPU tensor's pages live in a writable private file mapping,
  by resolving its `data_ptr()` against a snapshot of `/proc/self/maps`, and
  publish that decision as `tensor_needs_staging()` for callers that have to
  re-home weights themselves (accelerate CPU offload moves them outside any
  seam this module controls);
- stage only such tensors through anonymous memory, one tensor at a time, so
  peak host RSS grows by the size of the largest single tensor and not by the
  size of the whole checkpoint;
- offer the same operation for a whole `nn.Module` and, for third-party loaders
  that move the model internally (surya, diffusers pipelines), as a temporary
  patch of `torch.nn.Module.to`.

Key functions:
- `mmap_staging_required()`
- `tensor_needs_staging()`
- `stage_cpu_tensor()`
- `move_module_to()`
- `patched_module_to()` (context manager)
- `invalidate_maps_cache()`

Measurements (ROCm 7.2, gfx1201, torch 2.12, Surya recognition checkpoint):
- safetensors-mapped tensor -> `cuda`, plain copy: 2.0 s per tensor,
  independent of tensor size (the cost follows the mapping, not the copy);
- the same tensor via `clone()` first: 0.03 s;
- the same tensor with a dtype change in the same call: 0.05 s - Torch casts
  on the host into anonymous memory, so such a copy never hits the pathology;
- a hand-made mapping of the same file, 32 MiB slice, pages faulted in:
  `rw-p` 16.0 s, `r--p` 0.005 s, `rw-s` 0.008 s. The pathology is therefore
  *specific to writable private* file mappings, which is what the maps parser
  keys on.

Notes:
- A strict no-op on CUDA/CPU/MPS/absent-Torch builds, on non-posix hosts, for
  non-`cuda` targets, for non-CPU sources, for copies that also change dtype,
  for tensors below 1 MiB and for tensors outside a writable private file
  mapping. Staging an anonymous tensor would *triple* its transfer time
  (~0.16 s/GiB of pure memcpy), so the detection is not optional.
- `MS_ROCM_MMAP_STAGING=0` disables the workaround entirely (kill switch for
  diagnostics or once the driver is fixed).
- There is no public Torch API for "is this storage a file mapping":
  `UntypedStorage.filename` is `None` even for a safetensors-backed tensor, so
  `/proc/self/maps` is the only reliable signal. Parsing ~1700 mappings costs
  ~2 ms, one lookup ~0.2 us.
- The `/proc/self/maps` snapshot lives no longer than one *load session* (a
  `patched_module_to` block or a single `move_module_to` call) and is
  thread-local. A snapshot therefore cannot outlive the mappings it describes,
  which is what makes the classification correct by construction: an address
  freed and re-mapped by a *later* load is never answered from an older
  snapshot. Callers do not have to invalidate anything.
- The workaround is best-effort infrastructure: if `/proc/self/maps` cannot be
  read, or if the anonymous staging copy cannot be allocated, it degrades to
  stock Torch behaviour with a warning. Failures of the *transfer itself* are
  never swallowed.
"""

from __future__ import annotations

import bisect
import contextlib
import functools
import logging
import os
import threading
from typing import TYPE_CHECKING, Any, Callable, Iterator, NamedTuple

if TYPE_CHECKING:  # pragma: no cover - typing only, torch is imported lazily
    import torch

log = logging.getLogger(__name__)

# Below this size the stall was never observed, and the unconditional copy would
# cost more than it saves.
_MIN_STAGE_BYTES = 1 << 20

# Kill switch: `MS_ROCM_MMAP_STAGING=0` restores stock Torch transfers.
_STAGING_ENV_FLAG = "MS_ROCM_MMAP_STAGING"

_MAPS_PATH = "/proc/self/maps"


class _MapsSnapshot(NamedTuple):
    """Sorted parallel arrays describing this process' address space.

    `starts[i]`/`ends[i]` are the half-open bounds of mapping `i` and
    `writable_private[i]` tells whether that mapping is a *writable private file
    mapping* (`rw-p` over a real path), the only kind that triggers the amdkfd
    stall. Anonymous memory, read-only (`r--p`) and shared (`rw-s`) file
    mappings are all `False`. Sorted by `starts`, so a lookup is a single
    `bisect`.
    """

    starts: list[int]
    ends: list[int]
    writable_private: list[bool]


class _ThreadState(threading.local):
    """Per-thread staging state.

    `patch_depth` counts the `patched_module_to` blocks this thread has entered.
    The `torch.nn.Module.to` patch is necessarily a process-global class
    attribute, so this counter is what keeps it from changing the behaviour of
    threads that never asked for it: the replacement delegates to stock Torch
    whenever it is zero.

    `session_depth` counts open load sessions and `maps_snapshot` holds the
    `/proc/self/maps` snapshot shared by the lookups inside them. The snapshot
    is dropped when the outermost session ends, which bounds its lifetime to a
    span during which no model is unloaded and no mapping is therefore recycled.

    Subclassing `threading.local` is what makes `__init__` run once per thread,
    so every thread starts from zeroed counters and no snapshot.
    """

    def __init__(self) -> None:
        self.patch_depth = 0
        self.session_depth = 0
        self.maps_snapshot: _MapsSnapshot | None = None


_thread_state = _ThreadState()

# Sticky and process-wide: `/proc/self/maps` does not become readable again
# within one process, so a failure disables the workaround instead of re-failing
# per tensor. Guarded by `_unavailable_lock` so the warning is emitted once.
_unavailable_lock = threading.Lock()
_maps_unavailable = False

# Cached ROCm-build detection. `None` = not determined yet (Torch may not be
# imported yet, so a negative result is only cached once Torch imported fine).
_rocm_build: bool | None = None

# Guards installing/restoring the process-global `torch.nn.Module.to` patch.
_patch_lock = threading.Lock()
_patch_depth = 0
_patch_original: Callable[..., Any] | None = None

_activation_lock = threading.Lock()
_activation_logged = False


def _parse_maps_line(line: str) -> tuple[int, int, bool] | None:
    """Parse one `/proc/self/maps` row into `(lo, hi, writable_private_file)`.

    The third element is `True` only for a writable private mapping of a real
    filesystem path, because that is the only mapping kind the amdkfd stall
    applies to (see the measurement matrix in the file header).

    Returns `None` for a row that does not follow the documented
    `addr-addr perms offset dev inode [path]` layout and for a row whose range
    is empty or inverted, so a single odd row can neither invalidate the whole
    snapshot nor create an interval that swallows unrelated addresses.
    """
    rng, _, rest = line.partition(" ")
    lo_s, sep, hi_s = rng.partition("-")
    if not sep:
        return None
    try:
        lo = int(lo_s, 16)
        hi = int(hi_s, 16)
    except ValueError:
        return None
    if hi <= lo:
        # A zero-length or inverted range covers no address; keeping it would
        # only risk shadowing the neighbouring mapping in the bisect.
        return None
    # perms, offset, dev, inode, then the optional path as the remainder.
    fields = rest.split(None, 4)
    if len(fields) < 4:
        return None
    perms = fields[0]
    if len(perms) < 4:
        return None
    path = fields[4].strip() if len(fields) >= 5 else ""
    # A bracketed name ("[heap]", "[stack]", "[anon:...]") denotes anonymous
    # memory; only a real filesystem path means file-backed pages.
    file_backed = bool(path) and not path.startswith("[")
    # perms is "rwxp"/"rwxs": index 1 is the write bit, index 3 the sharing mode.
    return lo, hi, file_backed and perms[1] == "w" and perms[3] == "p"


def _read_maps() -> _MapsSnapshot | None:
    """Snapshot `/proc/self/maps`, or `None` when it cannot be read.

    `None` means "no information available" (non-Linux, hardened kernel, fd
    exhaustion); callers must then fall back to stock transfers rather than
    guessing.
    """
    rows: list[tuple[int, int, bool]] = []
    try:
        with open(_MAPS_PATH, "rb") as handle:
            for raw in handle:
                parsed = _parse_maps_line(raw.decode("utf-8", "replace"))
                if parsed is None:
                    log.debug("Skipping unparsable %s row: %r", _MAPS_PATH, raw[:120])
                    continue
                rows.append(parsed)
    except OSError as exc:
        log.debug("Cannot read %s: %s", _MAPS_PATH, exc)
        return None
    rows.sort()
    return _MapsSnapshot(
        starts=[row[0] for row in rows],
        ends=[row[1] for row in rows],
        writable_private=[row[2] for row in rows],
    )


def _lookup(snapshot: _MapsSnapshot, ptr: int) -> bool | None:
    """Whether `ptr` sits in a writable private file mapping; `None` if unmapped.

    `None` is distinct from `False`: it means the snapshot does not describe
    `ptr` at all (the region was mapped after it was taken) and the caller
    should refresh it.
    """
    index = bisect.bisect_right(snapshot.starts, ptr) - 1
    if index < 0:
        return None
    if ptr >= snapshot.ends[index]:
        return None
    return snapshot.writable_private[index]


def _mark_maps_unavailable(reason: str) -> None:
    """Disable the workaround process-wide, warning exactly once.

    `reason` is appended to the user-facing warning to distinguish "was never
    readable" from "became unreadable mid-run".
    """
    global _maps_unavailable
    with _unavailable_lock:
        if _maps_unavailable:
            return
        _maps_unavailable = True
    log.warning(
        "%s %s; the ROCm mmap->GPU staging workaround is disabled for this "
        "process and weight transfers fall back to stock Torch behaviour "
        "(they may be slow on ROCm).",
        _MAPS_PATH,
        reason,
    )


def _begin_maps_session() -> None:
    """Open a load session on the calling thread.

    A session is the lifetime of the `/proc/self/maps` snapshot: it is built
    lazily on the first lookup inside the session and dropped when the outermost
    session ends. Sessions nest.
    """
    _thread_state.session_depth += 1


def _end_maps_session() -> None:
    """Close a load session; drops the snapshot when the outermost one ends."""
    state = _thread_state
    state.session_depth -= 1
    if state.session_depth <= 0:
        state.session_depth = 0
        state.maps_snapshot = None


@contextlib.contextmanager
def _maps_session() -> Iterator[None]:
    """Scope a load session around a block, releasing the snapshot on exit."""
    _begin_maps_session()
    try:
        yield
    finally:
        _end_maps_session()


def _fresh_snapshot() -> _MapsSnapshot | None:
    """Read `/proc/self/maps` and cache it for the current session, if any.

    Returns `None` once the file has been found unreadable, which also disables
    the workaround for the rest of the process. Outside a session the snapshot
    is not retained: a lone `stage_cpu_tensor` call has no bounded window during
    which the address space is known to be stable, so it pays the ~2 ms parse
    every time rather than risk a stale answer.
    """
    snapshot = _read_maps()
    if snapshot is None:
        _mark_maps_unavailable("is not readable")
        return None
    state = _thread_state
    if state.session_depth > 0:
        state.maps_snapshot = snapshot
    return snapshot


def _is_file_backed(ptr: int) -> bool:
    """Whether `ptr` lies in a writable private file mapping of this process.

    Uses the current session's snapshot and re-reads it exactly once when `ptr`
    is not covered by it, which happens when a checkpoint was mapped after the
    snapshot was taken. Deliberately not memoized per `data_ptr()`: a freed
    address can be handed out again for a mapping of a different kind, which
    would turn a memoized answer into a wrong one.

    Returns `False` (i.e. "do not stage") when `/proc/self/maps` is unreadable.
    """
    if _maps_unavailable:
        return False

    state = _thread_state
    snapshot = state.maps_snapshot
    if snapshot is None:
        snapshot = _fresh_snapshot()
        if snapshot is None:
            return False

    found = _lookup(snapshot, ptr)
    if found is None:
        # Stale snapshot: the checkpoint was mapped after it was taken.
        snapshot = _fresh_snapshot()
        if snapshot is None:
            return False
        found = _lookup(snapshot, ptr)
    return bool(found)


def invalidate_maps_cache() -> None:
    """Drop the `/proc/self/maps` snapshot held by this thread's load session.

    Correctness no longer depends on this call: a snapshot never outlives the
    load session that built it (see `_ThreadState`), so a model unloaded after
    the session cannot be classified from stale data. It is kept as a manual
    escape hatch for diagnostics and for code that unmaps a checkpoint *inside*
    an open session. It affects only the calling thread and does not re-enable
    the workaround if `/proc/self/maps` was already found unreadable.
    """
    _thread_state.maps_snapshot = None


def _is_rocm_build() -> bool:
    """Whether the imported Torch is a ROCm/HIP build (cached per process)."""
    global _rocm_build
    if _rocm_build is not None:
        return _rocm_build
    try:
        import torch  # type: ignore
    except Exception:
        # Torch absent or not importable yet (ONNX-only install, import order):
        # report "not ROCm" without caching, so a later call can still decide.
        return False
    hip_version = getattr(getattr(torch, "version", None), "hip", None)
    _rocm_build = isinstance(hip_version, str) and bool(hip_version.strip())
    return _rocm_build


def mmap_staging_required() -> bool:
    """Whether the ROCm mmap->GPU staging workaround must be applied at all.

    `True` only for a ROCm/HIP Torch build on a posix host with the kill switch
    unset. The environment variable is re-read on every call so it can be
    flipped before a load without restarting the backend; the Torch build check
    is cached. Never raises.
    """
    if os.environ.get(_STAGING_ENV_FLAG, "").strip() == "0":
        return False
    if os.name != "posix":
        # /proc/self/maps is Linux-only, and the pathology is amdkfd-specific.
        return False
    return _is_rocm_build()


def _log_activation_once(nbytes: int) -> None:
    """Emit one info line the first time a tensor is actually staged."""
    global _activation_logged
    with _activation_lock:
        if _activation_logged:
            return
        _activation_logged = True
    log.info(
        "ROCm mmap->GPU staging active: file-backed CPU weights (first hit: "
        "%.1f MiB) are materialized in anonymous memory before the host->device "
        "copy to avoid the amdkfd copy-on-write stall. Set %s=0 to disable.",
        nbytes / (1 << 20),
        _STAGING_ENV_FLAG,
    )


def _should_stage(tensor: "torch.Tensor", device: "torch.device") -> bool:
    """Whether this exact copy hits the amdkfd pathology and needs staging.

    Checks are ordered cheapest-first: the ROCm/kill-switch gate, then the
    device pair, then the size threshold, and only then the `/proc/self/maps`
    lookup. `device` must already be normalized by `_normalize_device`, and the
    caller must have handled dtype-changing copies (they are cast on the host
    and never reach the pathology).
    """
    if not mmap_staging_required():
        return False
    if device.type != "cuda" or tensor.device.type != "cpu":
        return False
    if tensor.numel() * tensor.element_size() < _MIN_STAGE_BYTES:
        return False
    return _is_file_backed(tensor.data_ptr())


def tensor_needs_staging(tensor: Any) -> bool:
    """Whether copying **this one tensor** to the GPU would hit the amdkfd stall.

    The predicate a caller needs to answer "is it worth materializing these
    weights in anonymous memory myself?" - for instance before
    `enable_model_cpu_offload`, whose lazy `module.to(<gpu>)` would otherwise
    pay the stall during generation. `True` means the tensor is a CPU tensor of
    at least 1 MiB living in a writable private file mapping on a ROCm build
    with the workaround enabled, i.e. exactly the condition `stage_cpu_tensor`
    stages on. It is a thin wrapper over that decision, not a second copy of it.

    The gate (`mmap_staging_required()`) and the 1 MiB threshold are folded in
    deliberately, rather than exposing a bare "is this memory file-backed":
    every question a caller actually has is "would staging pay off here", and a
    caller who had to remember to and-in the gate itself could silently round
    trip gigabytes on a CUDA or CPU host. A `False` therefore always means
    "nothing to gain", never "I do not know".

    Scope: it answers about one tensor. A module is not "file-backed" as a
    whole; probe one representative tensor (loaders fill a component from a
    single file in one pass, so its weights are either all mapped or all
    copied) rather than iterating.

    Cost: outside a load session each call re-reads `/proc/self/maps` (~2 ms),
    because a snapshot of unknown age cannot be trusted after a model unload.
    Calling it per tensor over a whole checkpoint is therefore a mistake; a
    handful of probes is what it is for. `move_module_to` and
    `patched_module_to` open a session internally, so the per-tensor lookups of
    an actual load share one snapshot.

    # Raises
    `TypeError` if `tensor` is not a `torch.Tensor`.
    """
    import torch  # type: ignore

    if not isinstance(tensor, torch.Tensor):
        raise TypeError(
            f"tensor_needs_staging expects a torch.Tensor, got {type(tensor).__name__}"
        )
    # An index-less `cuda` device is enough for the device-pair check and does
    # not initialize a CUDA context, so the probe stays free on a CPU-only host.
    return _should_stage(tensor, torch.device("cuda"))


def _normalize_device(device: Any, tensor: "torch.Tensor") -> "torch.device":
    """Coerce `device` to `torch.device` and fill in an implied CUDA index.

    `torch.device("cuda")` means "the current CUDA device", so an index-less
    target must be resolved before it can be compared with the tensor's own
    device. The resolution is done only when the tensor already sits on a CUDA
    device, because `torch.cuda.current_device()` initializes a CUDA context and
    must not be triggered on a CPU-only path.
    """
    import torch  # type: ignore

    dev = device if isinstance(device, torch.device) else torch.device(device)
    if dev.type == "cuda" and dev.index is None and tensor.device.type == "cuda":
        dev = torch.device("cuda", torch.cuda.current_device())
    return dev


def _stock_tensor_to(
    tensor: "torch.Tensor", device: Any, dtype: Any, non_blocking: bool
) -> "torch.Tensor":
    """The exact per-tensor call stock `nn.Module.to` makes.

    `device` may be `None` (dtype-only conversion). Kept as one helper so every
    non-staged path is byte-for-byte the stock overload, including the
    `non_blocking` argument and Torch's own "return `self` when nothing changes"
    identity guarantee.
    """
    return tensor.to(device, dtype, non_blocking)


def stage_cpu_tensor(
    tensor: Any, device: Any, dtype: Any = None, *, non_blocking: bool = False
) -> Any:
    """Move one tensor to `device`, routing file-backed sources via anon memory.

    `device` may be `None`, in which case only an optional `dtype` conversion is
    performed. The returned tensor is the same object whenever the requested
    move and cast are no-ops, because every non-staged path delegates to the
    stock `Tensor.to(device, dtype, non_blocking)` overload.

    Only a >=1 MiB CPU tensor living in a writable private file mapping and
    copied to a `cuda` target on a ROCm build, *without* a dtype change, is
    staged; everything else takes the plain `Tensor.to()` path, so this is a
    strict no-op elsewhere (see `mmap_staging_required`). A dtype change is
    excluded because Torch casts on the host into anonymous memory first, so the
    copy already avoids the pathology - and skipping it here also avoids the
    `/proc/self/maps` lookup.

    `non_blocking` is forwarded to the final device copy. That is safe for the
    staging buffer: a host->device copy out of pageable memory does not return
    before the source has been read, so releasing the staging copy right after
    cannot race the DMA. The anonymous staging copy is released before
    returning, which bounds the extra host RSS by the size of one tensor. Its
    layout follows `Tensor.clone()`'s `preserve_format` default, which is the
    same layout stock `Tensor.to()` produces.

    If the *staging copy* cannot be allocated the call degrades to a direct
    transfer with a warning - the workaround must never turn a load that stock
    Torch could complete into a failure. A failure of the *transfer itself* is
    logged with context and re-raised, never swallowed.

    Callers that stage many tensors should hold a load session
    (`move_module_to`, `patched_module_to`); a bare call outside one re-reads
    `/proc/self/maps` (~2 ms) rather than trusting a snapshot of unknown age.

    # Raises
    `TypeError` if `tensor` is not a `torch.Tensor`.
    """
    import torch  # type: ignore

    if not isinstance(tensor, torch.Tensor):
        raise TypeError(
            f"stage_cpu_tensor expects a torch.Tensor, got {type(tensor).__name__}"
        )

    if device is None:
        # Pure dtype conversion: no host->device copy, nothing to stage.
        return _stock_tensor_to(tensor, None, dtype, non_blocking)

    dev = _normalize_device(device, tensor)
    already_there = tensor.device.type == dev.type and (
        dev.index is None or tensor.device.index == dev.index
    )
    if already_there:
        return _stock_tensor_to(tensor, dev, dtype, non_blocking)

    if dtype is not None and tensor.dtype != dtype:
        # Torch casts on the host first, into freshly allocated anonymous
        # memory, so the device copy never reads the file mapping (measured:
        # 2.107 s -> 0.049 s for a 165 MiB `rw-p` tensor). Returning here also
        # keeps a dtype-converting load off the `/proc/self/maps` path entirely.
        return _stock_tensor_to(tensor, dev, dtype, non_blocking)

    if not _should_stage(tensor, dev):
        return _stock_tensor_to(tensor, dev, dtype, non_blocking)

    nbytes = tensor.numel() * tensor.element_size()
    _log_activation_once(nbytes)
    try:
        staged = tensor.clone()
    except (RuntimeError, MemoryError) as exc:
        # There is enough memory for the transfer but not for an extra host
        # copy. The optimization is optional; the load is not.
        log.warning(
            "ROCm mmap->GPU staging could not allocate a %.1f MiB anonymous copy "
            "(%s tensor -> %s): %s. Falling back to a direct transfer, which may "
            "be slow on ROCm.",
            nbytes / (1 << 20),
            tensor.dtype,
            dev,
            exc,
        )
        return _stock_tensor_to(tensor, dev, dtype, non_blocking)

    try:
        return _stock_tensor_to(staged, dev, dtype, non_blocking)
    except (RuntimeError, MemoryError) as exc:
        log.error(
            "ROCm mmap->GPU transfer failed for a %.1f MiB %s tensor -> %s: %s",
            nbytes / (1 << 20),
            tensor.dtype,
            dev,
            exc,
        )
        raise
    finally:
        # Release the anonymous staging copy before the next tensor is staged.
        del staged


def _reject_non_float_dtype(dtype: Any) -> None:
    """Reproduce stock `nn.Module.to`'s dtype validation.

    Stock `nn.Module.to` accepts floating-point and complex dtypes only and
    raises before touching a single tensor; `_apply_staged` must not be a way
    around that check, or an integer `dtype` would silently produce a module
    stock Torch would have refused to build. The message is copied verbatim from
    `torch/nn/modules/module.py` (verified against torch 2.12).

    Unlike stock Torch this does not re-emit the "complex modules are
    experimental" `UserWarning`; that warning is informational and duplicating
    its wording here would only rot.

    # Raises
    `TypeError` if `dtype` is a `torch.dtype` that is neither floating point nor
    complex. `dtype` of `None` is accepted (no cast requested).
    """
    if dtype is None:
        return
    if not (dtype.is_floating_point or dtype.is_complex):
        raise TypeError(
            "nn.Module.to only accepts floating point or complex "
            f"dtypes, but got desired dtype={dtype}"
        )


def _apply_staged(
    module: Any, device: Any, dtype: Any, non_blocking: bool = False
) -> Any:
    """Apply staged transfers to every tensor of `module` in place.

    A drop-in replacement for stock `Module.to`'s traversal: it validates
    `dtype` up front exactly like stock does, delegates the walk to
    `nn.Module._apply` (so parameter re-binding, buffers, gradients and weight
    tying behave identically) and reproduces stock's meta-tensor diagnostic.

    Weight tying is preserved exactly as far as stock `Module.to` preserves it:
    when the *same* `Parameter` object is registered in several places the
    second visit already sees it on the target device and returns it unchanged,
    so the tie holds. Two *distinct* parameters that merely share one storage
    are split into separate storages - by stock `Module.to` as well, which
    copies per tensor for the same reason (verified on CPU with torch 2.12).

    # Raises
    `TypeError` if `dtype` is neither floating point nor complex.
    """
    _reject_non_float_dtype(dtype)

    def convert(tensor: "torch.Tensor") -> "torch.Tensor":
        # Stock `Module.to` casts floating-point and complex tensors only;
        # integer buffers keep their dtype.
        want = (
            dtype
            if (dtype is not None and (tensor.is_floating_point() or tensor.is_complex()))
            else None
        )
        try:
            return stage_cpu_tensor(tensor, device, want, non_blocking=non_blocking)
        except NotImplementedError as exc:
            # Stock `Module.to` turns the bare meta-tensor error into an
            # actionable one; keep that, or a `low_cpu_mem_usage` load would
            # regress to a worse message just because we bypassed it.
            if str(exc) == "Cannot copy out of meta tensor; no data!":
                raise NotImplementedError(
                    f"{exc} Please use torch.nn.Module.to_empty() instead of "
                    "torch.nn.Module.to() when moving module from meta to a "
                    "different device."
                ) from None
            raise

    return module._apply(convert)


def _stock_module_to(module: Any, device: Any, dtype: Any) -> Any:
    """Delegate to stock `Module.to`, letting Torch own every diagnostic."""
    return module.to(device) if dtype is None else module.to(device=device, dtype=dtype)


def move_module_to(module: Any, device: Any, dtype: Any = None) -> Any:
    """`module.to(device, dtype)` with ROCm mmap staging. Returns `module`.

    Semantically equivalent to stock `Module.to(device, dtype)` for the
    device+dtype form. It is a plain delegation to `module.to(...)` unless all
    of the following hold: `mmap_staging_required()`, `dtype` is a `torch.dtype`
    or `None`, and `device` resolves to a `cuda` target. Anything else - a
    non-ROCm host, a `cpu`/`mps` target, an argument Torch alone knows how to
    interpret - goes to stock Torch, so its validation and error messages are
    never bypassed.

    The `cuda` path opens a load session, so one `/proc/self/maps` snapshot is
    shared by the whole module and released when the call returns.

    `non_blocking` and `memory_format` are not supported; use `module.to(...)`
    directly if you need them.

    # Raises
    Whatever stock `Module.to` raises, including `TypeError` for a non
    floating-point/complex `dtype`.
    """
    if not mmap_staging_required():
        return _stock_module_to(module, device, dtype)

    import torch  # type: ignore

    if dtype is not None and not isinstance(dtype, torch.dtype):
        return _stock_module_to(module, device, dtype)
    try:
        dev = device if isinstance(device, torch.device) else torch.device(device)
    except (TypeError, ValueError, RuntimeError):
        # Not a device spec at all (a `Tensor`, `None`, garbage): stock Torch
        # owns the overload resolution and the diagnostics.
        return _stock_module_to(module, device, dtype)
    if dev.type != "cuda":
        # The pathology is amdkfd-specific; a non-cuda target must keep the
        # stock code path, validation included.
        return _stock_module_to(module, device, dtype)

    with _maps_session():
        return _apply_staged(module, dev, dtype)


def _make_patched_to(
    original: Callable[..., Any], parse_to: Callable[..., Any]
) -> Callable[..., Any]:
    """Build the `nn.Module.to` replacement installed by `patched_module_to`.

    The replacement is installed process-globally (there is no other way to
    intercept a third-party loader), but it is *armed per thread*: a thread that
    never entered a `patched_module_to` block delegates to `original` before
    anything else happens, so a concurrent load or inference elsewhere in the
    backend keeps stock behaviour. That check also makes restoring the attribute
    race-free: a call already inside `patched` keeps using the `original` it
    closed over.

    Of the armed calls, only one resolving to a `cuda` device without a
    `memory_format` is re-routed; every other form (dtype-only, `cpu`,
    unparsable arguments) delegates to `original` unchanged. It calls
    `_apply_staged` rather than `move_module_to`, because the latter would
    delegate back to `Module.to` and recurse while the patch is installed.
    """

    @functools.wraps(original)
    def patched(module: Any, *args: Any, **kwargs: Any) -> Any:
        if _thread_state.patch_depth <= 0:
            # This thread is not inside a `patched_module_to` block; it must not
            # observe the patch at all.
            return original(module, *args, **kwargs)
        try:
            device, dtype, non_blocking, memory_format = parse_to(*args, **kwargs)
        except (TypeError, RuntimeError) as exc:
            # Signature we do not understand: stock Torch owns the diagnostics.
            log.debug("Module.to(%r, %r) not parsable, using stock path: %s", args, kwargs, exc)
            return original(module, *args, **kwargs)
        if device is None or device.type != "cuda" or memory_format is not None:
            return original(module, *args, **kwargs)
        return _apply_staged(module, device, dtype, non_blocking)

    # Marker used on exit to detect that somebody else replaced our patch.
    patched._ms_rocm_staging_patch = True  # type: ignore[attr-defined]
    return patched


class patched_module_to:  # noqa: N801 - used as a context manager, not a type
    """Route this thread's `nn.Module.to(<cuda>)` through mmap staging.

    Needed for third-party loaders that move the model themselves (surya,
    `DiffusionPipeline.to`, `transformers` `device_map` handling): patching the
    `torch.nn.Module.to` class attribute also covers subclass overrides that end
    in `super().to(...)` (`PreTrainedModel`, diffusers `ModelMixin`) and
    `DiffusionPipeline.to`, which calls `module.to(device, dtype)` per
    component.

    Scope: the class attribute is replaced process-wide, but the replacement is
    armed only for threads inside a `with` block. Two consequences worth
    knowing: another thread loading or running inference concurrently is
    unaffected, and a loader that moves weights from *its own* worker threads
    keeps stock (slow on ROCm) behaviour - only the entering thread is covered.

    Contract: hold it **around model loading only, never around inference**.
    While it is held, every `Module.to(<cuda>)` on this thread pays a
    `/proc/self/maps` lookup per >=1 MiB tensor, which is pure overhead for
    inference-time moves of activations.

    The block also delimits a load session: one `/proc/self/maps` snapshot is
    taken lazily inside it and dropped when the outermost block exits, so no
    lookup can ever be answered from a snapshot older than the current load.

    Thread-safety: install/restore is serialized by a module lock and reference
    counted, so nested and concurrent `with` blocks are safe - the original
    `Module.to` is restored only when the last block in the process exits. If
    foreign code replaced `Module.to` in the meantime, the foreign
    implementation is left in place and a warning is logged instead of silently
    clobbering it.

    `enabled` defaults to `mmap_staging_required()`; pass an explicit value to
    force the patch on or off (tests, diagnostics). A disabled block is inert:
    it neither arms nor disarms an enclosing enabled block.
    """

    def __init__(self, enabled: bool | None = None) -> None:
        self._enabled = mmap_staging_required() if enabled is None else bool(enabled)
        # Whether *this* instance holds a reference on the global patch.
        self._active = False

    def __enter__(self) -> "patched_module_to":
        global _patch_depth, _patch_original

        if not self._enabled:
            return self
        try:
            import torch  # type: ignore
        except Exception as exc:
            log.warning("Torch is not importable; ROCm mmap staging patch skipped: %s", exc)
            self._enabled = False
            return self

        # `_parse_to` is the same private helper stock `Module.to` uses to
        # normalize its overloaded signature; without it we cannot tell a
        # device move from a dtype cast, so we do not patch at all.
        parse_to = getattr(getattr(getattr(torch, "_C", None), "_nn", None), "_parse_to", None)
        if parse_to is None:
            log.warning(
                "torch._C._nn._parse_to is unavailable in Torch %s; ROCm mmap "
                "staging patch skipped, transfers use stock behaviour.",
                getattr(torch, "__version__", "?"),
            )
            self._enabled = False
            return self

        with _patch_lock:
            if _patch_depth == 0:
                _patch_original = torch.nn.Module.to
                torch.nn.Module.to = _make_patched_to(_patch_original, parse_to)
            _patch_depth += 1
        # Arm the patch for this thread only, and open the load session that
        # bounds the `/proc/self/maps` snapshot lifetime.
        _thread_state.patch_depth += 1
        _begin_maps_session()
        self._active = True
        return self

    def __exit__(self, *exc_info: Any) -> bool:
        global _patch_depth, _patch_original

        if not self._active:
            return False
        self._active = False
        import torch  # type: ignore

        # Disarm this thread first: from here on this thread must see stock
        # behaviour even if another thread keeps the attribute patched.
        _thread_state.patch_depth -= 1
        _end_maps_session()

        with _patch_lock:
            _patch_depth -= 1
            if _patch_depth > 0:
                # Another block (nested here or in another thread) is still
                # active: keep the patch installed.
                return False
            original = _patch_original
            _patch_original = None
            if original is None:
                return False
            if getattr(torch.nn.Module.to, "_ms_rocm_staging_patch", False):
                torch.nn.Module.to = original
            else:
                log.warning(
                    "torch.nn.Module.to was replaced by other code while the ROCm "
                    "mmap staging patch was installed; leaving the foreign "
                    "implementation in place."
                )
        # Never suppress an exception raised inside the block.
        return False
