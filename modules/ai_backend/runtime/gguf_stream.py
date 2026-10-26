"""
File: modules/ai_backend/runtime/gguf_stream.py

Purpose:
Model-agnostic GGUF container parsing: read the header of a GGUF file (version,
metadata key/values, tensor infos) with plain `read()` calls and turn every
tensor info into the same `TensorSpan` a safetensors header yields, so the
streaming reader, the large-model inventory and the residency hooks treat both
containers alike.

Main responsibilities:
- the one GGUF header parser (`read_gguf_header` -> `GgufHeader`) and its size
  ceiling (`MAX_GGUF_HEADER_BYTES`);
- the GGUF magic sniff (`is_gguf_file`, `GGUF_MAGIC`);
- ggml type naming for messages (`ggml_type_name`).

Span contract (what a GGUF tensor becomes):
- GGUF stores dims in ggml order (`ne[0]` is the innermost, contiguous axis);
  a span's `shape` is the REVERSED list, i.e. torch order.
- A plain type (F32 / F16 / BF16 / F64 / I8 / I16 / I32 / I64) maps to its own
  token in the caller's `dtypes` table, exactly like safetensors.
- Every block-quantized type (Q*, K-quants, IQ*, TQ*) maps to `U8` with
  `quant_type` = the ggml type id and shape `(*rows, bytes_per_row)`, where
  `bytes_per_row = ne[0] // block_size * type_size`: one row of a quantized
  2-D weight is one contiguous run of whole blocks.
- `start` / `end` are relative to `GgufHeader.data_start` (the aligned end of
  the tensor-info section), like safetensors `data_offsets`.

Notes:
- Policy-free: an IQ or TQ type parses fine. Whether a consumer can dequantize
  a type is decided by the consumer (`gguf_layers`, the model packages).
- Plain `read()` on a buffered handle, never `mmap`: header parsing touches
  only the header, and the tensor bytes are read later by
  `safetensors_stream.StreamingTensorReader` into anonymous memory (which is
  also why no ROCm staging is needed for them).
- Metadata arrays (tokenizer vocabularies, merges) are skipped, not kept:
  only scalars and strings land in `GgufHeader.metadata`.
- A tensor with a zero dim (real files carry such markers, e.g. ComfyUI's
  `__index_timestep_zero__`) holds no bytes: it gets no span and is listed in
  `GgufHeader.empty_tensors`. A model that expects it then fails loudly as a
  missing weight; nothing substitutes an empty tensor.
- Stdlib only; the `gguf` package's `constants` module (type ids, block sizes)
  is imported lazily inside the parse, and its absence is a `RuntimeError`
  naming the package.
- Parse work is bounded, not only allocation: every count field is checked
  against a cardinality cap and against the bytes left (at a conservative
  minimum size per item) BEFORE its loop, and the whole header walks at most
  `_MAX_HEADER_ITEMS` items, so a crafted count fails at once.
- Errors are `ValueError` worded like `safetensors_stream`'s, naming the file
  and, for a tensor, the tensor.
"""

from __future__ import annotations

import logging
import struct
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path
from types import MappingProxyType
from typing import IO, Any

from .safetensors_stream import SAFETENSORS_DTYPES, TensorSpan, check_span_layout

log = logging.getLogger(__name__)

#: The four bytes every GGUF file starts with.
GGUF_MAGIC = b"GGUF"

#: Upper bound on the header (key/values plus tensor infos) we are willing to
#: parse. A Qwen3 GGUF with its 151k-token vocabulary and merges is ~7 MiB.
MAX_GGUF_HEADER_BYTES = 100 * 1024 * 1024

#: GGUF versions this parser reads. v1 used 32-bit lengths and is obsolete.
SUPPORTED_GGUF_VERSIONS = (2, 3)

#: Default `general.alignment` when the file does not set one (GGUF spec).
DEFAULT_GGUF_ALIGNMENT = 32

#: Sanity bound on a tensor's dimension count. ggml itself addresses at most 4
#: (`GGML_MAX_DIMS`), but GGUF writers outside llama.cpp store more (ComfyUI-GGUF
#: keeps 5-D conv3d weights, measured on a HunyuanImage file) and `gguf`'s own
#: reader accepts them; the container stays policy-free and only stops a crafted
#: count from driving an endless dims loop.
_MAX_DIMS = 8

#: Nesting bound for metadata arrays of arrays (legal GGUF, never deep in practice).
_MAX_ARRAY_DEPTH = 8

#: Cardinality caps on the header's count fields. The byte ceiling alone bounds
#: allocation, not parse WORK: 100 MiB of 8-byte empty strings is ~13 M Python
#: iterations. Measured real files: a Qwen3 GGUF has 35 key/values, 398
#: tensors and ~152 k-item tokenizer arrays; large LLM vocabularies stay well
#: under 1 M. Each cap leaves an order of magnitude of headroom.
_MAX_KV_COUNT = 65_536
_MAX_TENSOR_COUNT = 1 << 20
_MAX_ARRAY_ITEMS = 1 << 21
#: Whole-header work budget: key/values + tensor infos + every string or
#: nested-array item that must be walked one by one (scalar arrays are skipped
#: in one seek and cost nothing). A Qwen3 header walks ~455 k items; the cap
#: keeps the worst LEGAL header (4 M empty strings, 32 MiB) near 1.5 s.
_MAX_HEADER_ITEMS = 1 << 22

#: Conservative MINIMUM encoded size of one item behind each count field, used
#: to refuse a count that cannot fit in the remaining header bytes before the
#: loop starts: a key/value is an empty key (u64 length) + value type (u32) +
#: the smallest scalar (1 B); a tensor info is an empty name + n_dims (u32) +
#: one dim (u64) + type (u32) + offset (u64); a string item its u64 length; a
#: nested array its item type (u32) + count (u64).
_MIN_KV_BYTES = 8 + 4 + 1
_MIN_TENSOR_INFO_BYTES = 8 + 4 + 8 + 4 + 8
_MIN_STRING_BYTES = 8
_MIN_NESTED_ARRAY_BYTES = 4 + 8

#: The metadata key that overrides the data-section alignment.
_ALIGNMENT_KEY = "general.alignment"

#: ggml type names that are plain element types: they map to the safetensors
#: token of the same name in the caller's `dtypes` table.
_PLAIN_TYPE_NAMES = frozenset({"F32", "F16", "BF16", "F64", "I8", "I16", "I32", "I64"})

#: GGUF metadata value type id -> struct format of a fixed-size scalar (GGUF
#: spec, `gguf_type`). 8 = STRING and 9 = ARRAY are variable-sized.
_SCALAR_FORMATS: dict[int, str] = {
    0: "<B",  # UINT8
    1: "<b",  # INT8
    2: "<H",  # UINT16
    3: "<h",  # INT16
    4: "<I",  # UINT32
    5: "<i",  # INT32
    6: "<f",  # FLOAT32
    7: "<?",  # BOOL
    10: "<Q",  # UINT64
    11: "<q",  # INT64
    12: "<d",  # FLOAT64
}
_STRING_TYPE = 8
_ARRAY_TYPE = 9
_UINT32_TYPE = 4


@dataclass(frozen=True)
class GgufHeader:
    """A parsed GGUF header.

    `data_start` is the absolute offset of the tensor data section (every
    span's `start` / `end` is relative to it); `alignment` the data alignment
    every tensor offset is a multiple of; `spans` the tensors IN FILE ORDER
    (sorted by offset, checked for overlap; the file-size bound is the
    reader's);
    `metadata` the scalar and string key/values (arrays skipped);
    `empty_tensors` the names of zero-element tensors, which get no span.
    """

    version: int
    alignment: int
    data_start: int
    spans: tuple[TensorSpan, ...]
    metadata: Mapping[str, Any]
    empty_tensors: tuple[str, ...] = ()


def _gguf_constants() -> Any:
    """`gguf.constants`, or a `RuntimeError` naming the missing package."""
    try:
        from gguf import constants
    except ImportError as exc:
        raise RuntimeError(
            "Для чтения GGUF нужен пакет Python «gguf», он не установлен "
            f"({exc}). Переустановите окружение ИИ-бэкенда."
        ) from exc
    return constants


def ggml_type_name(quant_type: int) -> str:
    """Human name of a ggml type id (`"Q4_K"`), or `"type <id>"` for an id `gguf` does not know.

    # Raises
    `RuntimeError` when the `gguf` package is missing.
    """
    constants = _gguf_constants()
    try:
        return constants.GGMLQuantizationType(quant_type).name
    except ValueError:
        return f"type {quant_type}"


def is_gguf_file(path: Path) -> bool:
    """Whether `path` starts with the GGUF magic (a 4-byte sniff; nothing else is checked).

    # Raises
    `ValueError` when the file cannot be opened or read — a missing file is
    never silently "not GGUF".
    """
    try:
        with Path(path).open("rb") as handle:
            return handle.read(len(GGUF_MAGIC)) == GGUF_MAGIC
    except OSError as exc:
        raise ValueError(f"Не удалось прочитать {path}: {exc}") from exc


class _HeaderCursor:
    """Sequential little-endian reads over a GGUF header with truncation and ceiling checks.

    `position` is the absolute offset of the next byte. Every read and skip is
    checked against the file size (truncated header) and against
    `MAX_GGUF_HEADER_BYTES` (implausible header), so a hostile length field
    fails with a `ValueError` instead of a huge allocation. Every count field
    goes through `claim_items` before its loop, so a hostile count fails
    without driving millions of iterations either.
    """

    def __init__(self, handle: IO[bytes], path: Path, file_size: int) -> None:
        self._handle = handle
        self._path = path
        self._file_size = file_size
        self.position = 0
        self._items = 0

    def claim_items(self, count: int, min_item_bytes: int, cap: int, what: str) -> None:
        """Preflight a count field before looping over its `count` items.

        Refuses, in this order: a count above `cap`; a count whose items, at
        `min_item_bytes` each, cannot fit in the bytes left before the file end
        or `MAX_GGUF_HEADER_BYTES` (same wording as a truncated / oversized
        header); and a count that would push the whole header past
        `_MAX_HEADER_ITEMS` walked items. Reads nothing.
        """
        if count > cap:
            raise ValueError(f"Некорректный заголовок GGUF ({what}: {count} больше предела {cap}): {self._path}")
        self._claim(count * min_item_bytes, what)
        self._items += count
        if self._items > _MAX_HEADER_ITEMS:
            raise ValueError(
                f"Некорректный заголовок GGUF ({what}: всего элементов заголовка больше "
                f"предела {_MAX_HEADER_ITEMS}): {self._path}"
            )

    def _claim(self, count: int, what: str) -> None:
        """Check that `count` more bytes exist and stay under the ceiling."""
        end = self.position + count
        if end > MAX_GGUF_HEADER_BYTES:
            raise ValueError(
                f"Некорректный заголовок GGUF ({what}: заголовок длиннее "
                f"{MAX_GGUF_HEADER_BYTES} B): {self._path}"
            )
        if end > self._file_size:
            raise ValueError(f"Обрезанный заголовок GGUF ({what}): {self._path}")

    def read(self, count: int, what: str) -> bytes:
        """Exactly `count` bytes."""
        self._claim(count, what)
        data = self._handle.read(count)
        if len(data) != count:
            raise ValueError(f"Обрезанный заголовок GGUF ({what}): {self._path}")
        self.position += count
        return data

    def skip(self, count: int, what: str) -> None:
        """Advance `count` bytes without keeping them."""
        self._claim(count, what)
        self._handle.seek(count, 1)
        self.position += count

    def scalar(self, fmt: str, what: str) -> Any:
        """One `struct` scalar."""
        return struct.unpack(fmt, self.read(struct.calcsize(fmt), what))[0]

    def u32(self, what: str) -> int:
        """One little-endian uint32."""
        return int(self.scalar("<I", what))

    def u64(self, what: str) -> int:
        """One little-endian uint64."""
        return int(self.scalar("<Q", what))

    def string(self, what: str) -> str:
        """A GGUF string: uint64 length, then UTF-8 bytes."""
        length = self.u64(what)
        raw = self.read(length, what)
        try:
            return raw.decode("utf-8")
        except UnicodeDecodeError as exc:
            raise ValueError(f"Некорректный заголовок GGUF ({what}: строка не UTF-8): {self._path}") from exc

    def skip_string(self, what: str) -> None:
        """Skip one GGUF string."""
        self.skip(self.u64(what), what)


def _skip_array(cursor: _HeaderCursor, path: Path, key: str, depth: int = 0) -> None:
    """Skip one metadata array value (its item type and count were not yet read)."""
    item_type = cursor.u32(f"тип элементов массива «{key}»")
    count = cursor.u64(f"длина массива «{key}»")
    if item_type in _SCALAR_FORMATS:
        cursor.skip(count * struct.calcsize(_SCALAR_FORMATS[item_type]), f"массив «{key}»")
    elif item_type == _STRING_TYPE:
        cursor.claim_items(count, _MIN_STRING_BYTES, _MAX_ARRAY_ITEMS, f"длина массива «{key}»")
        for _ in range(count):
            cursor.skip_string(f"массив «{key}»")
    elif item_type == _ARRAY_TYPE:
        # Nested arrays are legal GGUF; the depth bound only stops a crafted
        # file from recursing without end.
        if depth >= _MAX_ARRAY_DEPTH:
            raise ValueError(f"Некорректный заголовок GGUF (слишком глубокий массив «{key}»): {path}")
        cursor.claim_items(count, _MIN_NESTED_ARRAY_BYTES, _MAX_ARRAY_ITEMS, f"длина массива «{key}»")
        for _ in range(count):
            _skip_array(cursor, path, key, depth + 1)
    else:
        raise ValueError(f"Некорректный заголовок GGUF (неизвестный тип {item_type} в массиве «{key}»): {path}")


def _read_metadata(cursor: _HeaderCursor, path: Path, count: int) -> tuple[dict[str, Any], int]:
    """Read `count` key/values; return the kept scalars/strings and the alignment."""
    metadata: dict[str, Any] = {}
    seen: set[str] = set()
    alignment = DEFAULT_GGUF_ALIGNMENT
    cursor.claim_items(count, _MIN_KV_BYTES, _MAX_KV_COUNT, "число ключей")
    for _ in range(count):
        key = cursor.string("ключ метаданных")
        if key in seen:
            raise ValueError(f"Некорректный заголовок GGUF (ключ «{key}» повторяется): {path}")
        seen.add(key)
        value_type = cursor.u32(f"тип значения «{key}»")
        if value_type in _SCALAR_FORMATS:
            value = cursor.scalar(_SCALAR_FORMATS[value_type], f"значение «{key}»")
            metadata[key] = value
        elif value_type == _STRING_TYPE:
            metadata[key] = cursor.string(f"значение «{key}»")
        elif value_type == _ARRAY_TYPE:
            _skip_array(cursor, path, key)
            continue
        else:
            raise ValueError(f"Некорректный заголовок GGUF (неизвестный тип значения {value_type} у «{key}»): {path}")
        if key == _ALIGNMENT_KEY:
            if value_type != _UINT32_TYPE:
                raise ValueError(f"Некорректный заголовок GGUF («{key}» должен быть uint32): {path}")
            alignment = int(metadata[key])
            # ggml requires a power of two (`gguf.cpp`: alignment & (alignment - 1)).
            if alignment <= 0 or alignment & (alignment - 1):
                raise ValueError(f"Некорректный заголовок GGUF (выравнивание {alignment} не степень двойки): {path}")
    return metadata, alignment


def _tensor_error(path: Path, name: str, detail: str) -> ValueError:
    """One wording for every malformed tensor info, so the file and tensor always appear."""
    return ValueError(f"Некорректная запись тензора «{name}» в GGUF {path}: {detail}")


def _span_of(
    path: Path,
    name: str,
    dims: list[int],
    type_id: int,
    offset: int,
    alignment: int,
    dtypes: Mapping[str, tuple[str, int]],
    constants: Any,
) -> TensorSpan:
    """Map one tensor info to a `TensorSpan` (module notes: span contract)."""
    try:
        ggml_type = constants.GGMLQuantizationType(type_id)
        block_size, type_size = constants.GGML_QUANT_SIZES[ggml_type]
    except (ValueError, KeyError) as exc:
        raise _tensor_error(path, name, f"неизвестный тип ggml {type_id}") from exc
    if offset % alignment:
        raise _tensor_error(path, name, f"смещение {offset} не кратно выравниванию {alignment}")
    rows = 1
    for dim in dims[1:]:
        rows *= dim
    torch_rows = tuple(reversed(dims[1:]))
    if ggml_type.name in _PLAIN_TYPE_NAMES and block_size == 1:
        token = ggml_type.name
        mapped = dtypes.get(token)
        if mapped is None:
            raise _tensor_error(
                path, name, f"неподдерживаемый тип данных «{token}» (известные: {', '.join(sorted(dtypes))})"
            )
        torch_dtype_name, item_bytes = mapped
        if item_bytes != type_size:
            raise _tensor_error(path, name, f"таблица типов даёт {item_bytes} B на элемент «{token}», а ggml — {type_size} B")
        shape = (*torch_rows, dims[0])
        nbytes = rows * dims[0] * item_bytes
        quant_type: int | None = None
    else:
        token = "U8"
        mapped = dtypes.get(token)
        if mapped is None or mapped[1] != 1:
            raise _tensor_error(path, name, f"квантованный тип {ggml_type.name} хранится как U8, а таблица типов его не содержит")
        torch_dtype_name = mapped[0]
        if dims[0] % block_size:
            raise _tensor_error(
                path, name, f"длина строки {dims[0]} не кратна размеру блока {block_size} типа {ggml_type.name}"
            )
        bytes_per_row = dims[0] // block_size * type_size
        shape = (*torch_rows, bytes_per_row)
        nbytes = rows * bytes_per_row
        quant_type = int(ggml_type)
    return TensorSpan(
        name=name,
        dtype_token=token,
        torch_dtype_name=torch_dtype_name,
        shape=shape,
        start=offset,
        end=offset + nbytes,
        quant_type=quant_type,
    )


def read_gguf_header(path: Path | str, *, dtypes: Mapping[str, tuple[str, int]] = SAFETENSORS_DTYPES) -> GgufHeader:
    """Parse the header of the GGUF file at `path` without reading any tensor byte.

    `dtypes` maps the plain element types to `(torch dtype attribute name,
    bytes per element)` as in `safetensors_stream.parse_tensor_spans`; a plain
    type absent from it is refused, and block-quantized types need `U8` in it.

    Checks, all before any tensor byte is read: magic, version 2 or 3, every
    length against the file size and `MAX_GGUF_HEADER_BYTES`, every count
    (key/values, tensor infos, string / nested array items) preflighted against
    a cardinality cap and the bytes left before its loop runs, unique metadata
    keys and tensor names, `general.alignment` a power-of-two uint32,
    1 <= n_dims <= 8, a known ggml type, a quantized row made
    of whole blocks, offsets multiples of the alignment, and the shared span
    layout rule (`check_span_layout`: no overlap). Spans are NOT checked
    against the file size (a header view; the reader does that).

    # Raises
    `ValueError` naming the file (and the tensor) for any malformed header;
    `RuntimeError` when the `gguf` package is missing.
    """
    source = Path(path)
    constants = _gguf_constants()
    try:
        file_size = int(source.stat().st_size)
        with source.open("rb") as handle:
            cursor = _HeaderCursor(handle, source, file_size)
            magic = cursor.read(len(GGUF_MAGIC), "сигнатура")
            if magic != GGUF_MAGIC:
                raise ValueError(f"Файл не является GGUF (сигнатура {magic!r}): {source}")
            version = cursor.u32("версия")
            if version not in SUPPORTED_GGUF_VERSIONS:
                raise ValueError(
                    f"Неподдерживаемая версия GGUF {version} (поддерживаются "
                    f"{', '.join(map(str, SUPPORTED_GGUF_VERSIONS))}, только little-endian): {source}"
                )
            tensor_count = cursor.u64("число тензоров")
            kv_count = cursor.u64("число ключей")
            metadata, alignment = _read_metadata(cursor, source, kv_count)
            infos: list[tuple[str, list[int], int, int]] = []
            names: set[str] = set()
            cursor.claim_items(tensor_count, _MIN_TENSOR_INFO_BYTES, _MAX_TENSOR_COUNT, "число тензоров")
            for _ in range(tensor_count):
                name = cursor.string("имя тензора")
                if name in names:
                    raise _tensor_error(source, name, "имя повторяется")
                names.add(name)
                n_dims = cursor.u32(f"число осей «{name}»")
                if not 1 <= n_dims <= _MAX_DIMS:
                    raise _tensor_error(source, name, f"число осей {n_dims} вне 1..{_MAX_DIMS}")
                dims = [cursor.u64(f"оси «{name}»") for _ in range(n_dims)]
                type_id = cursor.u32(f"тип «{name}»")
                offset = cursor.u64(f"смещение «{name}»")
                infos.append((name, dims, type_id, offset))
            header_end = cursor.position
    except OSError as exc:
        raise ValueError(f"Не удалось прочитать {source}: {exc}") from exc

    data_start = -(-header_end // alignment) * alignment
    # Zero-element tensors carry no bytes (module notes): no span, but named.
    empty = tuple(name for name, dims, _, _ in infos if 0 in dims)
    spans = [
        _span_of(source, name, dims, type_id, offset, alignment, dtypes, constants)
        for name, dims, type_id, offset in infos
        if 0 not in dims
    ]
    # No file-size bound here: the header is a layout view (header-only files
    # size fine); `StreamingTensorReader` checks every span against the file.
    ordered = check_span_layout(spans, source=source, container="GGUF")
    log.debug(
        "gguf header %s: v%d, %d tensors (+%d empty), %d kept metadata keys, data at %d (alignment %d)",
        source,
        version,
        len(ordered),
        len(empty),
        len(metadata),
        data_start,
        alignment,
    )
    return GgufHeader(
        version=version,
        alignment=alignment,
        data_start=data_start,
        spans=tuple(ordered),
        metadata=MappingProxyType(metadata),
        empty_tensors=empty,
    )
