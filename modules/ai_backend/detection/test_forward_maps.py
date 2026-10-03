"""
File: modules/ai_backend/detection/test_forward_maps.py

Unit tests for `forward_maps.py`: tile-batch validation and probability-map
quantization (the value contract of the `textdetector.*.forward` response).
"""

from __future__ import annotations

import numpy as np
import pytest

from modules.ai_backend.detection.forward_maps import (
    quantize_probability_maps,
    validate_tiles,
)


def test_validate_tiles_returns_dims() -> None:
    tiles = np.zeros((2, 64, 128, 3), dtype=np.uint8)
    assert validate_tiles(tiles, align=64, engine="ctd") == (2, 64, 128)


@pytest.mark.parametrize(
    "tiles",
    [
        np.zeros((0, 64, 64, 3), dtype=np.uint8),
        np.zeros((1, 64, 63, 3), dtype=np.uint8),
        np.zeros((1, 32, 64, 3), dtype=np.uint8),
        np.zeros((1, 64, 64, 4), dtype=np.uint8),
        np.zeros((64, 64, 3), dtype=np.uint8),
        np.zeros((1, 64, 64, 3), dtype=np.float32),
    ],
)
def test_validate_tiles_rejects_bad_batches(tiles: np.ndarray) -> None:
    with pytest.raises(ValueError):
        validate_tiles(tiles, align=64, engine="ctd")


def test_validate_tiles_rejects_non_array() -> None:
    with pytest.raises(ValueError):
        validate_tiles([[1, 2, 3]], align=4, engine="surya")  # type: ignore[arg-type]


def test_quantize_rounds_to_nearest_and_clips() -> None:
    # Values chosen away from exact .5 ties, which float32 cannot represent reliably.
    probs = np.array([[[[0.0, 1.0, 0.01, 0.004, -0.2, 1.7, 0.2]]]], dtype=np.float32)
    out = quantize_probability_maps(probs, engine="paddle")
    assert out.dtype == np.uint8
    assert out.shape == probs.shape
    assert out.flags["C_CONTIGUOUS"]
    assert out.ravel().tolist() == [0, 255, 3, 1, 0, 255, 51]


@pytest.mark.parametrize("bad", [np.nan, np.inf, -np.inf])
def test_quantize_rejects_non_finite(bad: float) -> None:
    probs = np.full((1, 1, 2, 2), 0.5, dtype=np.float32)
    probs[0, 0, 1, 1] = bad
    with pytest.raises(RuntimeError, match="non-finite"):
        quantize_probability_maps(probs, engine="surya")


def test_quantize_rounds_exact_ties_half_up() -> None:
    # 2.5 / 255 in float32 times 255 is exactly 2.5 in float32: an exact tie.
    # floor(x * 255 + 0.5) gives 3; a half-to-even `np.round` would give 2.
    tie = np.float32(0.009803921915590763)
    assert tie * np.float32(255.0) == np.float32(2.5)
    out = quantize_probability_maps(np.full((1, 1, 1, 1), tie, dtype=np.float32), engine="ctd")
    assert out.ravel().tolist() == [3]
