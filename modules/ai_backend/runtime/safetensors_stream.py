"""
File: modules/ai_backend/runtime/safetensors_stream.py

Purpose:
Model-agnostic checkpoint I/O: parse the safetensors JSON header, validate every
tensor entry against a dtype table, and read a checkpoint (safetensors, or GGUF
through the spans `gguf_stream` parses) ONE TENSOR AT A TIME with plain `read()`
calls, so a loader's host-memory peak is the largest tensor instead of the whole
file. Also the rate-limited byte-level progress reporter such a loader drives.

Main responsibilities:
- the one header parser (`read_safetensors_header_and_data_start`, with the
  header-only view `read_safetensors_header`) and its size ceiling
  (`MAX_SAFETENSORS_HEADER_BYTES`);
- the per-tensor contract (`TensorSpan`, `parse_tensor_spans`): mappable dtype,
  sane shape and offsets, length == shape x itemsize, no overlap, file order;
  `TensorSpan.quant_type` marks a block-quantized GGUF tensor (uint8 rows);
- the one span-layout rule of both containers (`check_span_layout`: file
  order, no overlap, inside the file);
- the container-agnostic reader (`StreamingTensorReader`) and its safetensors
  subclass (`StreamingSafetensorsReader`); which one a file gets is decided
  only by `tensor_container`;
- byte-level progress (`ByteProgress`, `ByteProgressCb`,
  `PROGRESS_MIN_INTERVAL_SECONDS`) and the bounded read chunk that feeds it
  inside one tensor (`READ_CHUNK_BYTES`);
- filling a module built on `meta` from one or more checkpoint files
  (safetensors or GGUF, via `tensor_container.open_tensor_reader`), one
  tensor at a time — THE one direct-to-device filler of the backend
  (`fill_module_from_safetensors`, `FillReport`, `MissingWeightsError`):
  1:1 (`key_map` + `cast`) or 1:N (`convert`, optionally `expected_keys_of`)
  key conversion, per-key placement (`targets`, `LEAVE_ON_META`) and an
  optional library writer (`write`, `TensorWrite`).

Key declarations:
- `SAFETENSORS_DTYPES` - the default dtype table; 8-bit float tokens are
  deliberately absent (see its comment). A caller that can handle more passes
  its own table through the `dtypes` parameters.
- `SAFETENSORS_DTYPES_WITH_F8` - the opt-in table with `F8_E4M3` / `F8_E5M2`.

Notes:
- No model-family names and no refusal POLICY live here: what a checkpoint may
  contain for one model (e.g. FLUX.2 klein's fp8-scaled refusal) stays in that
  model's package. The log wording of `ByteProgress` is owned by the caller
  through its `owner` / `logger` parameters, so each service's records keep its
  own prefix and land in its own logger tree.
- Why plain `read()` and not `safetensors.safe_open`: `safe_open` releases its
  pages only when the handle closes, so the peak still reaches the whole
  checkpoint. A `read()` into a `bytearray` plus `torch.frombuffer` leaves
  exactly one live tensor.
- torch is imported lazily inside `read_tensor` and
  `fill_module_from_safetensors`; everything else is stdlib, so header parsing
  and validation work without torch. `tensor_container` is imported lazily by
  the filler (it imports this module).
"""

from __future__ import annotations

import json
import logging
import struct
import time
from collections.abc import Callable, Iterable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any

log = logging.getLogger(__name__)

#: Upper bound on the JSON header of a safetensors file we are willing to parse.
MAX_SAFETENSORS_HEADER_BYTES = 100 * 1024 * 1024


# =====================================================================
#  The header
# =====================================================================
def read_safetensors_header_and_data_start(path: Path) -> tuple[dict[str, Any], int]:
    """Parse the JSON header AND report where the tensor data section begins.

    The second element is the absolute byte offset of the first tensor byte,
    i.e. `8 + header_len`: every `data_offsets` pair in the header is relative
    to it. A streaming reader needs both halves, so this is the full answer and
    `read_safetensors_header` is the header-only view of it — there is exactly
    one parser, and its `MAX_SAFETENSORS_HEADER_BYTES` ceiling and truncation
    checks apply to both callers.

    # Raises
    `ValueError` when the file is not a safetensors container or its header is
    implausibly large / not valid UTF-8 JSON.
    """
    try:
        with path.open("rb") as handle:
            raw_len = handle.read(8)
            if len(raw_len) != 8:
                raise ValueError(f"Файл слишком мал для safetensors: {path}")
            header_len = struct.unpack("<Q", raw_len)[0]
            if header_len == 0 or header_len > MAX_SAFETENSORS_HEADER_BYTES:
                raise ValueError(
                    f"Некорректный заголовок safetensors ({header_len} байт): {path}"
                )
            payload = handle.read(header_len)
    except OSError as exc:
        raise ValueError(f"Не удалось прочитать {path}: {exc}") from exc
    if len(payload) != header_len:
        raise ValueError(f"Обрезанный заголовок safetensors: {path}")
    try:
        header = json.loads(payload.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ValueError(f"Заголовок safetensors не является JSON: {path}") from exc
    if not isinstance(header, dict):
        raise ValueError(f"Заголовок safetensors не является объектом: {path}")
    return header, 8 + header_len


def read_safetensors_header(path: Path) -> dict[str, Any]:
    """Parse the JSON header of a safetensors file without loading any tensor.

    A thin view over `read_safetensors_header_and_data_start` for the callers
    that only ask what the file CONTAINS, not where it keeps it.

    # Raises
    `ValueError` when the file is not a safetensors container or its header is
    implausibly large / not valid UTF-8 JSON.
    """
    return read_safetensors_header_and_data_start(path)[0]


# =====================================================================
#  The safetensors container, one tensor at a time
# =====================================================================
#: Default safetensors dtype token -> (torch dtype attribute name, bytes per
#: element). The 8-bit float tokens are deliberately ABSENT: for FLUX.2 klein a
#: checkpoint carrying them is an fp8_scaled one, which its package recognizes
#: from the header and refuses with its own wording — a much better message than
#: "unsupported dtype". A caller that genuinely reads 8-bit floats passes its own
#: table through `dtypes`; this default must stay without them.
SAFETENSORS_DTYPES: dict[str, tuple[str, int]] = {
    "BOOL": ("bool", 1),
    "U8": ("uint8", 1),
    "I8": ("int8", 1),
    "I16": ("int16", 2),
    "U16": ("uint16", 2),
    "F16": ("float16", 2),
    "BF16": ("bfloat16", 2),
    "I32": ("int32", 4),
    "U32": ("uint32", 4),
    "F32": ("float32", 4),
    "I64": ("int64", 8),
    "U64": ("uint64", 8),
    "F64": ("float64", 8),
}

#: Opt-in table for loaders whose checkpoints legitimately carry 8-bit floats
#: (FLUX.3 klein's fp8r / nvfp4 exports). It is a SEPARATE table so that the
#: default above keeps refusing them for every caller that did not ask.
SAFETENSORS_DTYPES_WITH_F8: dict[str, tuple[str, int]] = {
    **SAFETENSORS_DTYPES,
    "F8_E4M3": ("float8_e4m3fn", 1),
    "F8_E5M2": ("float8_e5m2", 1),
}

#: The key safetensors reserves for the container's own metadata; it describes no
#: tensor and carries no `data_offsets`.
_METADATA_KEY = "__metadata__"


@dataclass(frozen=True)
class TensorSpan:
    """Where one tensor lives in a checkpoint file (safetensors or GGUF), and what it is.

    `start`/`end` are RELATIVE to the file's data section
    (`StreamingTensorReader.data_start`), never absolute: the header's own
    `data_offsets` for safetensors, the tensor-info offset for GGUF. `nbytes`
    is `end - start` and has already been checked against `shape` and the dtype's
    element size, so a reader may trust it.

    `quant_type` is the ggml block type id of a block-quantized GGUF tensor and
    `None` for every plain tensor. Such a span is `dtype_token="U8"` with shape
    `(*rows, bytes_per_row)` (`gguf_stream` module notes): its bytes travel as
    opaque uint8 rows, and only a consumer that knows the type dequantizes them.
    """

    name: str
    dtype_token: str
    torch_dtype_name: str
    shape: tuple[int, ...]
    start: int
    end: int
    quant_type: int | None = None

    @property
    def nbytes(self) -> int:
        """Length of this tensor's byte span in the file."""
        return self.end - self.start


def _span_error(source: Path, name: str, detail: str) -> ValueError:
    """One wording for every malformed tensor entry, so the file and key always appear."""
    return ValueError(f"Некорректная запись тензора «{name}» в {source}: {detail}")


def parse_tensor_spans(
    header: dict[str, Any],
    *,
    source: Path,
    dtypes: Mapping[str, tuple[str, int]] = SAFETENSORS_DTYPES,
) -> list[TensorSpan]:
    """Validate every tensor entry of `header` and return the spans IN FILE ORDER.

    File order — sorted by `data_offsets[0]` — is not cosmetic: a diffusion
    checkpoint is tens of GiB and may sit on a spinning disk, where reading in
    the header's (alphabetical) key order turns one sequential pass into random
    access across the whole file.

    `dtypes` maps a safetensors dtype token to `(torch dtype attribute name,
    bytes per element)`; a token absent from it is refused. The default is
    `SAFETENSORS_DTYPES`.

    Every entry is checked here rather than at read time, so a corrupt header
    fails before a single tensor byte is read: the dtype must be one `dtypes`
    can map, the shape must be non-negative integers, the offsets must be a
    non-negative ordered pair, the span must be non-empty, its length must equal
    `prod(shape) * itemsize`, and NO TWO SPANS MAY OVERLAP.

    Non-overlap is checked, full contiguity deliberately is NOT. Overlap is the
    one malformed layout that produces a silently WRONG result instead of an
    error: two expected weights aliasing the same bytes both land in `loaded`,
    `missing` stays empty, and the model runs on corrupted weights. A GAP between
    two spans cannot do that — this reader seeks absolutely to
    `data_start + span.start` for every tensor, so unreferenced bytes are simply
    never read — and demanding contiguity would refuse an alignment-padded
    container that this reader handles correctly.

    # Raises
    `ValueError` naming the file and the offending key; for an overlap, BOTH
    keys and the byte range they share.
    """
    spans: list[TensorSpan] = []
    for name, entry in header.items():
        if name == _METADATA_KEY:
            continue
        if not isinstance(entry, dict):
            raise _span_error(source, name, "запись не является объектом")

        token = str(entry.get("dtype", "")).upper()
        mapped = dtypes.get(token)
        if mapped is None:
            raise _span_error(
                source,
                name,
                f"неподдерживаемый тип данных «{entry.get('dtype')}» "
                f"(известные: {', '.join(sorted(dtypes))})",
            )
        torch_dtype_name, item_bytes = mapped

        raw_shape = entry.get("shape")
        if not isinstance(raw_shape, list) or not all(
            isinstance(dim, int) and not isinstance(dim, bool) and dim >= 0 for dim in raw_shape
        ):
            raise _span_error(source, name, f"некорректная форма {raw_shape!r}")
        shape = tuple(int(dim) for dim in raw_shape)

        offsets = entry.get("data_offsets")
        if not isinstance(offsets, list) or len(offsets) != 2 or not all(
            isinstance(value, int) and not isinstance(value, bool) for value in offsets
        ):
            raise _span_error(source, name, f"некорректные data_offsets {offsets!r}")
        start, end = int(offsets[0]), int(offsets[1])
        if start < 0 or end < start:
            raise _span_error(source, name, f"смещения идут вспять: {start}..{end}")
        if end == start:
            # Not skipped silently: an empty span means the file does not carry
            # this weight, and substituting an empty tensor would let the model
            # build with a hole in it.
            raise _span_error(source, name, "нулевая длина данных")

        elements = 1
        for dim in shape:
            elements *= dim
        declared = elements * item_bytes
        if declared != end - start:
            raise _span_error(
                source,
                name,
                f"длина данных {end - start} B не совпадает с формой {shape} "
                f"типа {token} ({declared} B)",
            )

        spans.append(
            TensorSpan(
                name=name,
                dtype_token=token,
                torch_dtype_name=torch_dtype_name,
                shape=shape,
                start=start,
                end=end,
            )
        )

    return check_span_layout(spans, source=source)


def check_span_layout(
    spans: Iterable[TensorSpan],
    *,
    source: Path,
    data_start: int = 0,
    file_size: int | None = None,
    container: str = "safetensors",
) -> list[TensorSpan]:
    """The one span-layout rule of both containers: sort by `start`, refuse overlaps and spans past the file.

    Returns the spans IN FILE ORDER (sorted by `start`; see
    `parse_tensor_spans` for why file order matters). Two spans sharing a byte
    are refused (the one malformed layout that loads silently wrong weights
    instead of failing); gaps are allowed (alignment padding). With
    `file_size`, every `data_start + end` must lie inside the file, so a
    reader built on the result can fail only on I/O; `None` skips that check
    (a header checked without its file). `container` names the format in the
    truncation message.

    # Raises
    `ValueError` naming the file and, for an overlap, BOTH tensors and their
    byte ranges; for a truncated file, the tensor and both sizes.
    """
    ordered = sorted(spans, key=lambda span: span.start)
    # Pairwise once the spans are ordered: with `start` ascending, an overlap can
    # only be with the immediate predecessor, so one pass is enough.
    for previous, current in zip(ordered, ordered[1:]):
        if current.start < previous.end:
            raise ValueError(
                f"Перекрывающиеся тензоры в {source}: «{previous.name}» занимает байты "
                f"{previous.start}..{previous.end}, а «{current.name}» — "
                f"{current.start}..{current.end}. Такой файл повреждён: один и тот же "
                "участок данных прочитался бы как два разных веса."
            )
    if file_size is not None:
        for span in ordered:
            if data_start + span.end > file_size:
                raise ValueError(
                    f"Обрезанный {container} {source}: тензор «{span.name}» объявлен до байта "
                    f"{data_start + span.end}, а файл занимает {file_size} B"
                )
    return ordered


def file_size_of(path: Path) -> int:
    """`st_size` of `path` as a refusal-style `ValueError` on an OS error."""
    try:
        return int(path.stat().st_size)
    except OSError as exc:
        raise ValueError(f"Не удалось получить размер {path}: {exc}") from exc


class StreamingTensorReader:
    """Reads a checkpoint file tensor by tensor with ordinary `read()` calls (container-agnostic).

    Built from already parsed `spans` (relative to `data_start`) of the file at
    `path` whose size is `file_size`; construction re-applies
    `check_span_layout` (file order, no overlap, inside the file), so
    `read_tensor` can fail only on I/O. Reads must happen inside the context
    manager; the file handle is unbuffered, because every read is already one
    whole tensor. `tensor_container.open_tensor_reader` picks the container;
    `StreamingSafetensorsReader` is the safetensors subclass. `container`
    (default: the class's `container`) names the format in the truncation
    message.

    Each returned tensor VIEWS its own freshly allocated `bytearray`, so exactly
    one tensor is alive in anonymous memory at a time — that is the whole point
    of this class, and it is why `safetensors.safe_open` (and `mmap` for GGUF)
    is not used: mapped pages are released only when the handle closes, so the
    peak still reaches the whole checkpoint. A block-quantized GGUF span comes
    back as its uint8 rows (`TensorSpan.quant_type`). A tensor is read in
    chunks of at most `READ_CHUNK_BYTES` straight into its own buffer, so a
    byte-progress caller hears from a single large tensor several times.

    # Raises
    `ValueError` (construction) for overlapping spans or a span past the end
    of the file.
    """

    #: Container name used in the truncation message.
    container = "файл весов"

    def __init__(
        self,
        path: Path | str,
        spans: Sequence[TensorSpan],
        data_start: int,
        file_size: int,
        *,
        container: str | None = None,
    ) -> None:
        self.path = Path(path)
        self.data_start = int(data_start)
        self.file_size = int(file_size)
        if container is not None:
            self.container = container
        self.spans = check_span_layout(
            spans, source=self.path, data_start=self.data_start, file_size=self.file_size, container=self.container
        )
        self._handle: Any = None

    def __enter__(self) -> StreamingTensorReader:
        self._handle = self.path.open("rb", buffering=0)
        return self

    def __exit__(self, *exc_info: object) -> None:
        if self._handle is not None:
            self._handle.close()
            self._handle = None

    @property
    def total_tensor_bytes(self) -> int:
        """Sum of every tensor's span — the denominator byte progress is reported against.

        Deliberately not the file size: the header and any padding are not
        tensors, so a fraction taken against `st_size` never reaches 1.0.
        """
        return sum(span.nbytes for span in self.spans)

    def keys_in_file_order(self) -> list[str]:
        """Tensor names ordered by their offset in the file. See `parse_tensor_spans`."""
        return [span.name for span in self.spans]

    def read_tensor(
        self,
        span: TensorSpan,
        *,
        on_bytes: Callable[[int], None] | None = None,
        chunk_bytes: int | None = None,
    ) -> Any:
        """Read one tensor's bytes and wrap them, without a copy, in a `torch.Tensor`.

        The result owns a `bytearray` of exactly `span.nbytes`; nothing else of
        the file stays in anonymous memory. The bytes are read with `readinto`
        on slices of that one buffer, at most `chunk_bytes` (default
        `READ_CHUNK_BYTES`) per call, and `on_bytes(n)` is called after every
        successful `readinto` with the `n` bytes it delivered, so the calls sum
        to `span.nbytes` on success. `on_bytes` runs on the calling thread;
        whatever it raises propagates.

        # Raises
        `RuntimeError` when called outside the context manager; `ValueError` when
        the file ends inside the tensor or the read fails.
        """
        import torch

        if self._handle is None:
            raise RuntimeError(
                f"{type(self).__name__}({self.path}) используется вне контекстного менеджера"
            )
        chunk = READ_CHUNK_BYTES if chunk_bytes is None else int(chunk_bytes)
        if chunk <= 0:
            raise ValueError(f"read_tensor: размер порции чтения должен быть положительным, получено {chunk}")
        length = span.nbytes
        buffer = bytearray(length)
        view = memoryview(buffer)
        try:
            self._handle.seek(self.data_start + span.start)
            read = 0
            while read < length:
                # A bounded slice of the destination buffer: no second
                # full-size copy, and the caller hears progress per chunk.
                got = self._handle.readinto(view[read : min(length, read + chunk)])
                if not got:
                    raise ValueError(
                        f"Файл {self.path} оборвался на тензоре «{span.name}»: прочитано "
                        f"{read} B из {length} B"
                    )
                read += got
                if on_bytes is not None:
                    on_bytes(got)
        except OSError as exc:
            raise ValueError(f"Ошибка чтения тензора «{span.name}» из {self.path}: {exc}") from exc
        tensor = torch.frombuffer(buffer, dtype=getattr(torch, span.torch_dtype_name))
        return tensor.reshape(span.shape)


class StreamingSafetensorsReader(StreamingTensorReader):
    """The safetensors `StreamingTensorReader`: parses and validates the header itself.

    Construction parses the header (`read_safetensors_header_and_data_start`,
    kept as `header`), validates every entry against `dtypes`
    (`parse_tensor_spans`; default `SAFETENSORS_DTYPES`) and checks every span
    against the file's real size.

    # Raises
    `ValueError` (construction) when the header is malformed, a dtype is not in
    `dtypes`, or a span runs past the end of the file.
    """

    container = "safetensors"

    def __init__(
        self, path: Path | str, *, dtypes: Mapping[str, tuple[str, int]] = SAFETENSORS_DTYPES
    ) -> None:
        source = Path(path)
        self.header, data_start = read_safetensors_header_and_data_start(source)
        spans = parse_tensor_spans(self.header, source=source, dtypes=dtypes)
        super().__init__(source, spans, data_start, file_size_of(source))


# =====================================================================
#  Byte-level progress
# =====================================================================
#: Byte-level progress callback: `(done_bytes, total_bytes, last_key)`.
ByteProgressCb = Callable[[int, int, str], None]

#: Smallest gap between two progress reports: five frames per second. A
#: diffusion checkpoint holds thousands of tensors and reporting each one would
#: be thousands of frames per load, while the UI computes the read speed from
#: the byte deltas of consecutive frames, so they must come at a steady pace.
#: Used only by checkpoint LOADS; downloads pace their own frames.
PROGRESS_MIN_INTERVAL_SECONDS = 0.2

#: Largest single `readinto` of `StreamingTensorReader.read_tensor`. Must stay
#: well below what the slowest supported disk reads in
#: `PROGRESS_MIN_INTERVAL_SECONDS` (~150 MB/s HDD -> ~30 MB): with one read per
#: tensor a 0.5-1.2 GB embedding tensor would freeze the bar for seconds.
READ_CHUNK_BYTES = 16 * 1024**2


class ByteProgress:
    """Rate-limits a `ByteProgressCb` and never lets it break the load.

    `advance` is called per read chunk (or per tensor); it reports at most every
    `interval_seconds` (the first report one interval after construction), plus
    unconditionally when `final=True`, so the bar always ends at 100%. A final
    `advance` that adds no bytes since the last report is skipped when that
    report already carried the same count (no duplicate last frame). A callback
    that raises is logged at debug and swallowed — a dead IPC peer must not kill
    a load that is otherwise succeeding.

    `owner` is the service name that prefixes that debug record, and `logger` is
    the logger it goes to, so the record keeps the caller's wording and lands in
    the caller's logger tree (`"<owner>: обработчик прогресса загрузки бросил
    исключение: <exc>"`).
    """

    def __init__(
        self,
        callback: ByteProgressCb | None,
        total_bytes: int,
        *,
        owner: str,
        logger: logging.Logger,
        interval_seconds: float = PROGRESS_MIN_INTERVAL_SECONDS,
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        self._owner = owner
        self._logger = logger
        self._callback = callback
        self._total = int(total_bytes)
        self._interval = float(interval_seconds)
        self._clock = clock
        self._done = 0
        self._last_report = clock()
        # `done` of the last report, `None` before the first one.
        self._reported_done: int | None = None

    @property
    def done_bytes(self) -> int:
        """Bytes of the checkpoint consumed so far."""
        return self._done

    def advance(self, nbytes: int, key: str, *, final: bool = False) -> None:
        """Count `nbytes` against the total and report if the interval has elapsed."""
        self._done += int(nbytes)
        if self._callback is None:
            return
        if final and self._reported_done == self._done:
            return
        now = self._clock()
        if not final and now - self._last_report < self._interval:
            return
        self._last_report = now
        self._reported_done = self._done
        try:
            self._callback(self._done, self._total, key)
        except Exception as exc:  # noqa: BLE001 - a dead peer must not kill the load
            self._logger.debug(
                "%s: обработчик прогресса загрузки бросил исключение: %s", self._owner, exc
            )



# =====================================================================
#  Filling a meta-built module
# =====================================================================
#: How many offending keys a refusal message lists; the count is always given.
_LISTED_KEYS = 5

#: What a `targets` callback returns for a state key that must stay on `meta`:
#: the filler neither writes it nor reads a file tensor that yields only such
#: keys, and the final no-`meta` check exempts it.
LEAVE_ON_META = "meta"

#: One converted tensor on its way into the module: `(state key, tensor, device)`.
TensorWrite = tuple[str, Any, Any]


@dataclass(frozen=True)
class FillReport:
    """What `fill_module_from_safetensors` did.

    `loaded` counts state keys written into the module; `skipped` counts file
    tensors never read because they yield no state key (`key_map` returned
    `None`, or `expected_keys_of` returned nothing); `bytes_read` is the sum of
    the spans actually read; `left_on_meta` counts state keys `targets` left on
    `meta` (provided by the files, deliberately not written).
    """

    loaded: int
    skipped: int
    bytes_read: int
    left_on_meta: int = 0


class MissingWeightsError(ValueError):
    """An expected persistent key of the module that no file provides.

    A `ValueError` (the filler's refusal type) carrying the sorted state keys in
    `missing`, so a caller that owns its own wording for an incomplete checkpoint
    can re-raise without parsing the message.
    """

    def __init__(self, message: str, missing: Sequence[str]) -> None:
        super().__init__(message)
        self.missing: tuple[str, ...] = tuple(missing)


def _listed(entries: Sequence[str]) -> str:
    """`count` plus the first `_LISTED_KEYS` entries, for a refusal message."""
    return f"{len(entries)} шт., первые: {', '.join(entries[:_LISTED_KEYS])}"


def _set_module_tensor(root: Any, key: str, tensor: Any) -> None:
    """Replace the parameter or buffer `key` of `root` with `tensor`.

    A parameter becomes `nn.Parameter(tensor, requires_grad=False)`; a buffer is
    reassigned on its owning submodule, which keeps its registration (and so its
    persistence flag). `key` is known to be a `state_dict` key of `root`.
    """
    import torch

    owner_path, _, leaf = key.rpartition(".")
    owner = root.get_submodule(owner_path) if owner_path else root
    if leaf in owner._parameters:
        setattr(owner, leaf, torch.nn.Parameter(tensor, requires_grad=False))
    else:
        # `Module.__setattr__` routes a tensor assigned to a registered buffer
        # name back into `_buffers`, so the buffer stays a buffer.
        setattr(owner, leaf, tensor)


def _source_label(path: Path, span_name: str, state_key: str | None) -> str:
    """`file:key` for a refusal, plus `-> state key` when a 1:N `convert` produced it."""
    label = f"{path.name}:{span_name}"
    return label if state_key is None else f"{label} -> {state_key}"


def _missing_error(owner: str, files: Sequence[Path], missing: Sequence[str]) -> MissingWeightsError:
    """The one wording for expected keys no file provides (header-time or after a 1:N fill)."""
    sources = ", ".join(path.name for path in files) or "(нет файлов)"
    return MissingWeightsError(
        f"{owner}: в файлах {sources} нет весов, нужных модели ({_listed(missing)}). "
        "Похоже, файлы неполные или от другой модели.",
        missing,
    )


@dataclass
class _FillState:
    """Mutable bookkeeping of one `fill_module_from_safetensors` call."""

    provider: dict[str, Path]
    meta_keys: set[str]
    loaded: int = 0
    left_on_meta: int = 0


def fill_module_from_safetensors(
    module: Any,
    files: Sequence[Path],
    *,
    device: Any,
    key_map: Callable[[str], str | None] | None = None,
    cast: Callable[[str, Any], Any] | None = None,
    convert: Callable[[str, Any], Iterable[tuple[str, Any]]] | None = None,
    expected_keys_of: Callable[[str], Sequence[str]] | None = None,
    targets: Callable[[str], Any] | None = None,
    write: Callable[[Sequence[TensorWrite]], None] | None = None,
    dtypes: Mapping[str, tuple[str, int]] = SAFETENSORS_DTYPES,
    progress: ByteProgressCb | None = None,
    progress_interval_seconds: float = PROGRESS_MIN_INTERVAL_SECONDS,
    log_summary: bool = True,
    owner: str,
    logger: logging.Logger,
) -> FillReport:
    """Fill every persistent parameter and buffer of a `meta`-built `module` from `files`.

    Contract:
    - `module` was built on the `meta` device; its NON-persistent buffers (they
      are not in any checkpoint) are already real tensors.
    - `files` are read in the given order, each in file order, ONE tensor at a
      time (`tensor_container.open_tensor_reader`: safetensors or GGUF; a
      block-quantized GGUF tensor arrives as its uint8 rows, see
      `TensorSpan.quant_type`): when a file tensor is read, no
      earlier one is still referenced by the filler, so the host peak is one
      source tensor plus whatever it converts into.
    - Key conversion, exactly one of two forms:
      - 1:1 — `key_map(file_key)` returns the module's `state_dict` key, or
        `None` to skip the tensor (never read); `cast(state_key, tensor)` (default:
        identity) runs on the CPU tensor before the move, and the stored dtype is
        whatever it returns (a quantized loader keeps fp8 / packed u8 as is);
      - 1:N — `convert(file_key, tensor)` returns the `(state_key, tensor)` pairs
        one CPU file tensor becomes (renamed, split, cast — its own business).
        With `expected_keys_of(file_key)` (the state keys `convert` will produce,
        known from the header alone) the key set is checked before any read and
        a file key declaring none is skipped unread; `convert` must then produce
        exactly the declared keys. Without it every file tensor is read and the
        key checks run per tensor, the missing-key check after the last one.
    - `targets(state_key)` (default: `device` for every key) names each key's
      device, or `LEAVE_ON_META`: such a key is not written and stays `meta`; a
      file tensor whose declared keys all stay `meta` is not read at all.
    - Writing: by default the shape is checked (after `cast` / `convert`, which
      may reshape) and the tensor is copied with `.to(key's device)`; a
      parameter becomes `nn.Parameter(t, requires_grad=False)`, a buffer is set
      on its owning submodule. `write(writes)` replaces that step for all
      `(state_key, tensor, device)` writes of ONE file tensor; it then owns the
      shape check and the placement (FLUX.2 klein's diffusers writer).
    - A CUDA/HIP device is synchronized after every file tensor, as measured
      for FLUX.2 klein: without it the host buffers of queued copies pile up on
      HIP. A CPU target has nothing to synchronize.
    - `progress` gets `(done_bytes, total_bytes, last_key)` as bytes arrive
      (per `READ_CHUNK_BYTES` chunk inside a tensor, `last_key` = the tensor
      being read) at most every `progress_interval_seconds`, and always once
      at 100% after the last tensor was written, through
      `ByteProgress(owner=owner, logger=logger)`, on the calling thread; the
      total counts only the tensors that will be read (one `(0, 0, "")`
      report when none is).
    - `log_summary=False` drops the start / finish INFO records for a caller
      that logs its own.

    The key set is checked from the HEADERS before any tensor byte is read
    whenever the keys are known there (unexpected key, the same state key from
    two tensors, an expected key no file provides), so a wrong checkpoint fails
    in milliseconds, not after gigabytes. On any refusal the module is left
    partially filled and must be discarded.

    The per-tensor host->device copy reads from anonymous memory (a
    `bytearray`), so it needs no ROCm staging: the amdkfd stall described in
    the `rocm_mmap_transfer.py` header only hits a copy whose source lives in
    a writable private FILE mapping (`rw-p`), which this filler never hands to
    the device, so wrapping it in `rocm_mmap_transfer.patched_module_to()`
    would only add a scan. Moving a WHOLE module elsewhere must still go
    through `rocm_mmap_transfer.move_module_to`.

    # Raises
    `TypeError` for an invalid combination of the conversion arguments.
    `ValueError` naming the file(s) and key(s) (the first five of each kind) for:
    a malformed header or a dtype not in `dtypes`; an unexpected key; a state key
    provided twice (across or within files); `convert` producing other keys than
    `expected_keys_of` declared; a shape mismatch (default writer); any
    parameter or buffer still on `meta` after the fill that `targets` did not
    leave there. An expected persistent key left unfilled is
    `MissingWeightsError` (a `ValueError`). I/O errors surface as `ValueError`
    from the reader; whatever `write` raises propagates unchanged. A GGUF file
    without the `gguf` package installed is a `RuntimeError`.
    """
    import torch

    if (key_map is None) == (convert is None):
        raise TypeError("fill_module_from_safetensors: нужен ровно один из key_map / convert")
    if convert is not None and cast is not None:
        raise TypeError("fill_module_from_safetensors: cast задаётся только вместе с key_map")
    if expected_keys_of is not None and convert is None:
        raise TypeError("fill_module_from_safetensors: expected_keys_of задаётся только вместе с convert")

    started = time.monotonic()
    target = torch.device(device)
    # Lazy: `tensor_container` imports this module (the dispatch sits above both containers).
    from .tensor_container import open_tensor_reader

    readers = [open_tensor_reader(path, dtypes=dtypes) for path in files]
    expected_shapes = {key: tuple(value.shape) for key, value in module.state_dict().items()}

    def place_of(state_key: str) -> Any:
        """The key's device, or `None` when `targets` leaves it on `meta`."""
        if targets is None:
            return target
        place = torch.device(targets(state_key))
        return None if place.type == LEAVE_ON_META else place

    def declared_keys(file_key: str) -> list[str] | None:
        """The state keys a file tensor yields, from the header alone; `None` = unknown."""
        if key_map is not None:
            state_key = key_map(file_key)
            return [] if state_key is None else [state_key]
        if expected_keys_of is not None:
            return list(expected_keys_of(file_key))
        return None

    # Plan the whole fill from the headers: which spans are read, and refuse a
    # wrong key set before the first read whenever the keys are known there.
    plan: list[tuple[StreamingTensorReader, list[tuple[TensorSpan, list[str] | None]]]] = []
    state = _FillState(provider={}, meta_keys=set())
    unexpected: list[str] = []
    duplicates: list[str] = []
    skipped = 0
    for reader in readers:
        wanted: list[tuple[TensorSpan, list[str] | None]] = []
        for span in reader.spans:
            keys = declared_keys(span.name)
            if keys is None:
                wanted.append((span, None))
                continue
            if not keys:
                skipped += 1
                continue
            for state_key in keys:
                # A 1:1 refusal keeps naming the file key alone, as it always has.
                label = _source_label(reader.path, span.name, None if key_map is not None else state_key)
                if state_key not in expected_shapes:
                    unexpected.append(label)
                    continue
                first = state.provider.get(state_key)
                if first is not None:
                    duplicates.append(f"{label} (уже из {first.name})")
                    continue
                state.provider[state_key] = reader.path
            on_meta = [state_key for state_key in keys if place_of(state_key) is None]
            if len(on_meta) == len(keys):
                # Every key this tensor yields stays on `meta`: not read at all.
                state.meta_keys.update(on_meta)
                state.left_on_meta += len(on_meta)
                continue
            wanted.append((span, keys))
        plan.append((reader, wanted))

    if unexpected:
        raise ValueError(
            f"{owner}: в чекпоинте есть веса, которых нет в модели ({_listed(unexpected)}). "
            "Похоже, это файл другой модели или другой версии."
        )
    if duplicates:
        raise ValueError(
            f"{owner}: один и тот же вес встречается в чекпоинте несколько раз ({_listed(duplicates)})."
        )
    keys_known_from_headers = convert is None or expected_keys_of is not None
    if keys_known_from_headers:
        missing = sorted(set(expected_shapes) - set(state.provider))
        if missing:
            raise _missing_error(owner, files, missing)

    read_count = sum(len(wanted) for _, wanted in plan)
    total_bytes = sum(span.nbytes for _, wanted in plan for span, _ in wanted)
    reporter = ByteProgress(
        progress, total_bytes, owner=owner, logger=logger, interval_seconds=progress_interval_seconds
    )
    if log_summary:
        logger.info(
            "%s: загрузка весов из %d файл(ов), %d тензоров (%.2f ГиБ) на %s",
            owner,
            len(readers),
            read_count,
            total_bytes / 1024**3,
            target,
        )

    def consume(reader: StreamingTensorReader, span: TensorSpan, declared: list[str] | None) -> None:
        """Read, convert, check, write and synchronize ONE file tensor.

        A function of its own so that every reference to the source tensor and
        to what it converted into (loop variables included) dies on return,
        before the next read: that is what keeps the host peak at one tensor.
        """
        tensor = reader.read_tensor(span, on_bytes=lambda nbytes: reporter.advance(nbytes, span.name))
        if convert is not None:
            pairs = list(convert(span.name, tensor))
        else:
            # 1:1: the plan holds `[key_map(file_key)]` for every span it reads.
            pairs = [
                (state_key, tensor if cast is None else cast(state_key, tensor))
                for state_key in declared or ()
            ]
        del tensor
        produced = [state_key for state_key, _ in pairs]
        if declared is not None and convert is not None and sorted(produced) != sorted(declared):
            raise ValueError(
                f"{owner}: преобразование веса «{span.name}» из {reader.path} дало ключи "
                f"{sorted(produced)}, а заголовок обещал {sorted(declared)}."
            )
        if declared is None:
            for state_key in produced:
                label = _source_label(reader.path, span.name, state_key)
                if state_key not in expected_shapes:
                    raise ValueError(
                        f"{owner}: в чекпоинте есть веса, которых нет в модели ({_listed([label])}). "
                        "Похоже, это файл другой модели или другой версии."
                    )
                first = state.provider.get(state_key)
                if first is not None:
                    raise ValueError(
                        f"{owner}: один и тот же вес встречается в чекпоинте несколько раз "
                        f"({_listed([f'{label} (уже из {first.name})'])})."
                    )
                state.provider[state_key] = reader.path
        writes: list[TensorWrite] = []
        for state_key, value in pairs:
            place = place_of(state_key)
            if place is None:
                state.meta_keys.add(state_key)
                state.left_on_meta += 1
            else:
                writes.append((state_key, value, place))
        del pairs
        if write is not None:
            if writes:
                write(writes)
        else:
            for state_key, value, place in writes:
                shape = tuple(value.shape)
                if shape != expected_shapes[state_key]:
                    raise ValueError(
                        f"{owner}: вес «{span.name}» в {reader.path} имеет форму {shape}, "
                        f"а модель ждёт {expected_shapes[state_key]} (ключ «{state_key}»)."
                    )
                _set_module_tensor(module, state_key, value.to(place))
        for place in {place for _, _, place in writes if place.type == "cuda"}:
            torch.cuda.synchronize(place)
        state.loaded += len(writes)

    index = 0
    last_key = ""
    for reader, wanted in plan:
        with reader:
            for span, declared in wanted:
                consume(reader, span, declared)
                index += 1
                last_key = span.name
                if index == read_count:
                    # The bytes were counted per chunk while reading; this is
                    # the guaranteed 100% frame, after the last write.
                    reporter.advance(0, span.name, final=True)
    if read_count == 0:
        reporter.advance(0, last_key, final=True)

    if not keys_known_from_headers:
        missing = sorted(set(expected_shapes) - set(state.provider))
        if missing:
            raise _missing_error(owner, files, missing)
    still_meta = [
        name
        for name, value in [*module.named_parameters(), *module.named_buffers()]
        if value.is_meta and name not in state.meta_keys
    ]
    if still_meta:
        raise ValueError(
            f"{owner}: после загрузки часть тензоров модели осталась на meta ({_listed(still_meta)}). "
            "Это ошибка сборки модели: непостоянные буферы должны быть созданы до загрузки."
        )
    if log_summary:
        logger.info(
            "%s: веса загружены: %d тензоров, пропущено %d, прочитано %.2f ГиБ за %.1f с",
            owner,
            state.loaded,
            skipped,
            reporter.done_bytes / 1024**3,
            time.monotonic() - started,
        )
    return FillReport(
        loaded=state.loaded,
        skipped=skipped,
        bytes_read=reporter.done_bytes,
        left_on_meta=state.left_on_meta,
    )
