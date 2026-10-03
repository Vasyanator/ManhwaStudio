"""
File: modules/ai_backend/detection/forward_maps.py

Purpose:
Input checks and output quantization shared by the three forward-only text
detectors (`ctd.py`, `paddle.py`, `surya.py`).

Main responsibilities:
- validate a tile batch (`uint8`, shape `[n, h, w, 3]`, `n >= 1`, sides a
  multiple of the engine's input alignment);
- turn a float probability batch `[n, C, mh, mw]` into the wire's `uint8`
  maps, refusing non-finite values instead of hiding them as zeros.

Key functions:
- validate_tiles()
- quantize_probability_maps()

Notes:
numpy only, no model runtime, so it is unit-tested directly
(`test_forward_maps.py`). The wire layout itself (header fields, blob order,
channel names) is owned by `ipc/handlers/textdetector.py`.
"""

from __future__ import annotations

import numpy as np


def validate_tiles(tiles: np.ndarray, *, align: int, engine: str) -> tuple[int, int, int]:
    """Check a tile batch and return its `(n, height, width)`.

    `tiles` must be a `uint8` array of shape `[n, height, width, 3]` (RGB,
    tile-major, row-major) with `n >= 1` and both sides a positive multiple of
    `align` (the engine's input alignment). `engine` only names the detector in
    error messages.

    Raises `ValueError` naming the offending property on any violation.
    """
    if not isinstance(tiles, np.ndarray):
        raise ValueError(f"{engine}: tiles must be a numpy array, got {type(tiles).__name__}.")
    if tiles.dtype != np.uint8:
        raise ValueError(f"{engine}: tiles must be uint8, got {tiles.dtype}.")
    if tiles.ndim != 4 or tiles.shape[3] != 3:
        raise ValueError(f"{engine}: tiles must have shape [n, h, w, 3], got {tuple(tiles.shape)}.")
    n, height, width = (int(v) for v in tiles.shape[:3])
    if n < 1:
        raise ValueError(f"{engine}: at least one tile is required.")
    if height < 1 or width < 1 or height % align != 0 or width % align != 0:
        raise ValueError(
            f"{engine}: tile size {width}x{height} must be a positive multiple of {align}."
        )
    return n, height, width


def quantize_probability_maps(maps: np.ndarray, *, engine: str) -> np.ndarray:
    """Quantize float probability maps to the wire's `uint8` form.

    `maps` is a float array `[n, C, mh, mw]` of probabilities. Returns a
    C-contiguous `uint8` array of the same shape holding
    `floor(clip(p, 0, 1) * 255 + 0.5)` computed in float32 (half up, never
    half-to-even). The Rust side quantizes native maps with the same float32
    `floor(x * 255 + 0.5)` form (`ms-onnx/src/paddle_ocr/mod.rs`), so both
    runtimes agree bit for bit.

    Raises `RuntimeError` when any value is NaN or infinite: a broken forward
    pass (for example fp16 overflow) must surface as an error, never as an empty
    map that silently detects nothing.
    """
    values = np.asarray(maps, dtype=np.float32)
    finite = np.isfinite(values)
    if not bool(finite.all()):
        bad = int(values.size - np.count_nonzero(finite))
        raise RuntimeError(
            f"{engine}: the detector returned {bad} non-finite probability values "
            f"out of {int(values.size)}."
        )
    scaled = np.clip(values, 0.0, 1.0) * np.float32(255.0)
    return np.ascontiguousarray(np.floor(scaled + np.float32(0.5)).astype(np.uint8))
