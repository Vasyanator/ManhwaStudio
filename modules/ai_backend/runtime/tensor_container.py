"""
File: modules/ai_backend/runtime/tensor_container.py

Purpose:
The ONE dispatch between the two checkpoint containers the backend reads,
safetensors and GGUF: which container a file is, its tensor layout (spans in
file order, data start, file size) and the tensor-at-a-time reader for it.
Every consumer that needs a file's tensors — the large-model inventory, the
residency hooks' DISK data start, the meta-module filler — goes through here,
so no site re-decides "is this GGUF".

Main responsibilities:
- `container_kind(path)` - a 4-byte magic sniff (`"gguf"` / `"safetensors"`);
- `ContainerLayout` / `read_container_layout(path, *, dtypes)` - the parsed
  layout of either container;
- `open_tensor_reader(path, *, dtypes)` - a `StreamingTensorReader` for the
  file (`StreamingSafetensorsReader` for safetensors).

Notes:
- safetensors has no magic (it starts with an 8-byte header length), so a file
  that is not GGUF is treated as safetensors and its own parser refuses junk.
- Policy-free and model-agnostic: block-quantized GGUF tensors are U8 spans
  with `TensorSpan.quant_type`; whether a model accepts them is its business.
- Imports no torch (a test pins this); `gguf` is imported lazily by
  `gguf_stream` only when a GGUF file is parsed.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

from .gguf_stream import GgufHeader, is_gguf_file, read_gguf_header
from .safetensors_stream import (
    SAFETENSORS_DTYPES,
    StreamingSafetensorsReader,
    StreamingTensorReader,
    TensorSpan,
    file_size_of,
    parse_tensor_spans,
    read_safetensors_header_and_data_start,
)

#: The two container kinds.
ContainerKind = Literal["safetensors", "gguf"]


@dataclass(frozen=True)
class ContainerLayout:
    """Where a checkpoint file keeps its tensors.

    `spans` are in file order and relative to `data_start` (absolute offset of
    the data section), validated and free of overlaps. They are NOT checked
    against `file_size`: a layout is a header view (sizing works on header-only
    files), and the bounds check is the reader's (`open_tensor_reader`), which
    is where reading a truncated file would go wrong. `gguf` is the full GGUF
    header (metadata, version, alignment) for a GGUF file, `None` for
    safetensors.
    """

    kind: ContainerKind
    spans: tuple[TensorSpan, ...]
    data_start: int
    file_size: int
    gguf: GgufHeader | None = None


def container_kind(path: Path | str) -> ContainerKind:
    """`"gguf"` when the file starts with the GGUF magic, else `"safetensors"`.

    # Raises
    `ValueError` when the file cannot be read.
    """
    return "gguf" if is_gguf_file(Path(path)) else "safetensors"


def read_container_layout(path: Path | str, *, dtypes: Mapping[str, tuple[str, int]] = SAFETENSORS_DTYPES) -> ContainerLayout:
    """Parse the header of a safetensors or GGUF file (no tensor byte is read).

    `dtypes` is the plain-dtype table both parsers validate against (a GGUF
    file's quantized tensors need `U8` in it).

    # Raises
    `ValueError` for an unreadable, malformed or truncated file (the
    container parser's wording); `RuntimeError` for a GGUF file without the
    `gguf` package.
    """
    source = Path(path)
    if container_kind(source) == "gguf":
        header = read_gguf_header(source, dtypes=dtypes)
        return ContainerLayout(
            kind="gguf",
            spans=header.spans,
            data_start=header.data_start,
            file_size=file_size_of(source),
            gguf=header,
        )
    raw_header, data_start = read_safetensors_header_and_data_start(source)
    spans = parse_tensor_spans(raw_header, source=source, dtypes=dtypes)
    return ContainerLayout(kind="safetensors", spans=tuple(spans), data_start=data_start, file_size=file_size_of(source))


def open_tensor_reader(path: Path | str, *, dtypes: Mapping[str, tuple[str, int]] = SAFETENSORS_DTYPES) -> StreamingTensorReader:
    """A tensor-at-a-time reader for a safetensors or GGUF file (enter it before reading).

    Safetensors gets `StreamingSafetensorsReader` (it keeps the parsed
    `header`); GGUF gets the base `StreamingTensorReader` over the spans of
    `read_gguf_header`. Either way every span is checked against the file
    size here, so a truncated file is refused before the first read.

    # Raises
    As `read_container_layout`.
    """
    source = Path(path)
    if container_kind(source) == "gguf":
        layout = read_container_layout(source, dtypes=dtypes)
        return StreamingTensorReader(source, layout.spans, layout.data_start, layout.file_size, container="GGUF")
    return StreamingSafetensorsReader(source, dtypes=dtypes)
