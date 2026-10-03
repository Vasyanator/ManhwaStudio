"""
File: modules/ai_backend/ipc/handlers/textdetector.py

Methods hosted here (forward-only text detection, PROTOCOL.md §5.3):
    textdetector.ctd.forward    — CTD network       (METHOD_TEXTDETECTOR_CTD_FORWARD)
    textdetector.paddle.forward — PP-OCR det network (METHOD_TEXTDETECTOR_PADDLE_FORWARD)
    textdetector.surya.forward  — Surya segformer    (METHOD_TEXTDETECTOR_SURYA_FORWARD)

Purpose:
Owns the wire contract of the three forward methods: the request
`{n, width, height}` + RGB tile blob, the response
`{engine, n, map_width, map_height, channels}` + `uint8` map blob, and every
validation on both directions. Rust (`crates/ms-text-detect`) plans, resizes,
tiles, stitches and post-processes; the services only run the network.

Key structures:
- ForwardSpec / FORWARD_SPECS — per-engine align, map stride, channel names.

Key functions:
- parse_forward_request()   — header + blob -> (n, width, height), validated.
- encode_forward_response() — service maps -> (response header, blob), validated.

Notes:
Not cancellable (one bounded forward pass). numpy is imported lazily inside the
handler so this package stays importable without the model stack.
"""

from __future__ import annotations

import threading
from dataclasses import dataclass
from typing import Any

from ..protocol import (
    MAX_BLOB_BYTES,
    METHOD_TEXTDETECTOR_CTD_FORWARD,
    METHOD_TEXTDETECTOR_PADDLE_FORWARD,
    METHOD_TEXTDETECTOR_SURYA_FORWARD,
)
from ..registry import HandlerContext, register


@dataclass(frozen=True)
class ForwardSpec:
    """Wire facts of one detector engine; mirrored by `ms_backend_ipc::textdetector`.

    `align`: both tile sides must be a positive multiple of it.
    `map_stride`: each map side is the tile side divided by it.
    `channels`: map names in blob order (one map per name per tile).
    """

    engine: str
    align: int
    map_stride: int
    channels: tuple[str, ...]


FORWARD_SPECS: dict[str, ForwardSpec] = {
    "ctd": ForwardSpec(engine="ctd", align=64, map_stride=1, channels=("seg", "shrink")),
    "paddle": ForwardSpec(engine="paddle", align=32, map_stride=1, channels=("prob",)),
    "surya": ForwardSpec(engine="surya", align=4, map_stride=4, channels=("text",)),
}


def _require_int(header: dict[str, Any], name: str) -> int:
    """Return header field `name` as an int; `bool` and non-integers are rejected."""
    value = header.get(name)
    if isinstance(value, bool) or not isinstance(value, int):
        raise ValueError(f"Field '{name}' must be an integer, got {value!r}.")
    return value


def parse_forward_request(spec: ForwardSpec, header: dict[str, Any], blob: bytes) -> tuple[int, int, int]:
    """Validate a forward request and return `(n, width, height)`.

    Rules: `n >= 1`; `width` and `height` positive multiples of `spec.align`;
    `len(blob) == n * height * width * 3` exactly (RGB u8, tile-major, row-major);
    the response the request implies fits `MAX_BLOB_BYTES`. Python ints do not
    overflow, so the products are exact; the explicit bound checks replace the
    checked arithmetic of the Rust mirror.

    Raises `ValueError` naming the violated rule (the dispatcher answers it as
    `status:"error"` with that message).
    """
    n = _require_int(header, "n")
    width = _require_int(header, "width")
    height = _require_int(header, "height")
    if n < 1:
        raise ValueError(f"{spec.engine}: field 'n' must be at least 1, got {n}.")
    if width < 1 or height < 1 or width % spec.align != 0 or height % spec.align != 0:
        raise ValueError(
            f"{spec.engine}: tile size {width}x{height} must be a positive multiple of {spec.align}."
        )
    expected = n * height * width * 3
    if expected > MAX_BLOB_BYTES:
        raise ValueError(
            f"{spec.engine}: {n} tiles of {width}x{height} need {expected} bytes, over MAX_BLOB_BYTES {MAX_BLOB_BYTES}."
        )
    if len(blob) != expected:
        raise ValueError(
            f"{spec.engine}: request blob is {len(blob)} bytes, expected {expected} "
            f"(n={n} x {height} x {width} x 3)."
        )
    response_bytes = n * len(spec.channels) * (height // spec.map_stride) * (width // spec.map_stride)
    if response_bytes > MAX_BLOB_BYTES:
        raise ValueError(
            f"{spec.engine}: the response for {n} tiles of {width}x{height} would be {response_bytes} bytes, "
            f"over MAX_BLOB_BYTES {MAX_BLOB_BYTES}; send fewer tiles per request."
        )
    return n, width, height


def encode_forward_response(spec: ForwardSpec, n: int, width: int, height: int, maps: Any) -> tuple[dict[str, Any], bytes]:
    """Check the service's maps and build the response header and blob.

    `maps` must be a `uint8` array of shape `[n, C, height / stride, width / stride]`
    with `C == len(spec.channels)`. The blob is `maps` in C order: tile-major,
    then channel-major, then row-major.

    Raises `RuntimeError` when the service output breaks that contract or the
    blob exceeds `MAX_BLOB_BYTES` (a service defect, not a client error).
    """
    map_width = width // spec.map_stride
    map_height = height // spec.map_stride
    expected_shape = (n, len(spec.channels), map_height, map_width)
    shape = tuple(int(v) for v in getattr(maps, "shape", ()))
    if shape != expected_shape:
        raise RuntimeError(f"{spec.engine}: detector maps have shape {shape}, expected {expected_shape}.")
    dtype = str(getattr(maps, "dtype", ""))
    if dtype != "uint8":
        raise RuntimeError(f"{spec.engine}: detector maps must be uint8, got {dtype or type(maps).__name__}.")
    blob = maps.tobytes(order="C")
    if len(blob) > MAX_BLOB_BYTES:
        raise RuntimeError(f"{spec.engine}: response blob {len(blob)} bytes exceeds MAX_BLOB_BYTES {MAX_BLOB_BYTES}.")
    header = {
        "engine": spec.engine,
        "n": n,
        "map_width": map_width,
        "map_height": map_height,
        "channels": list(spec.channels),
    }
    return header, blob


def _forward(spec: ForwardSpec, service: Any, header: dict[str, Any], blob: bytes) -> tuple[dict[str, Any], bytes]:
    """Shared body of the three handlers: validate, run the service, encode."""
    n, width, height = parse_forward_request(spec, header, blob)
    # Lazy: keeps `ipc/` importable without numpy-backed services loaded.
    import numpy as np

    tiles = np.frombuffer(blob, dtype=np.uint8).reshape(n, height, width, 3)
    maps = service.forward_tiles(tiles)
    return encode_forward_response(spec, n, width, height, maps)


def _handle_textdetector_ctd_forward(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`textdetector.ctd.forward`: CTD maps `[seg, shrink]` at the tile resolution."""
    return _forward(FORWARD_SPECS["ctd"], ctx.state.text_detector_ctd, header, blob)


def _handle_textdetector_paddle_forward(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`textdetector.paddle.forward`: PP-OCR DB probability map at the tile resolution."""
    return _forward(FORWARD_SPECS["paddle"], ctx.state.text_detector_paddle, header, blob)


def _handle_textdetector_surya_forward(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`textdetector.surya.forward`: Surya text heatmap at a quarter of the tile size."""
    return _forward(FORWARD_SPECS["surya"], ctx.state.text_detector_surya, header, blob)


register(METHOD_TEXTDETECTOR_CTD_FORWARD, _handle_textdetector_ctd_forward)
register(METHOD_TEXTDETECTOR_PADDLE_FORWARD, _handle_textdetector_paddle_forward)
register(METHOD_TEXTDETECTOR_SURYA_FORWARD, _handle_textdetector_surya_forward)
