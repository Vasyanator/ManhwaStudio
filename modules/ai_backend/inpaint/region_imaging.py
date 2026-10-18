"""
File: modules/ai_backend/inpaint/region_imaging.py

Purpose:
The wire codecs of the region editors (FLUX.2 klein, FLUX.3 klein): decoding the
region and the mask off the wire, validating a `whole_region` mask, and encoding
the answer. Model-agnostic: PNG bytes in, numpy arrays out, and back.

The post-processing of a region result (colour match, feathered composite, mask
morphology) is not done here: the backend answers the RAW decoded region and the
Rust host owns every step after the model (`crates/ms-region-postprocess`).

Main responsibilities:
- wire decoding (`decode_image_rgb`, `decode_mask`, `require_solid_mask`);
- wire encoding (`encode_png_rgb`).

Notes:
numpy and PIL are imported lazily inside the functions, so importing this module
stays cheap.
"""

from __future__ import annotations

import io
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    import numpy as np


# =====================================================================
#  Image / mask codecs
# =====================================================================
def decode_image_rgb(image_bytes: bytes) -> np.ndarray:
    """Decode any PIL-readable image to a contiguous HxWx3 uint8 RGB array (PIL errors propagate)."""
    import numpy as np
    from PIL import Image

    with Image.open(io.BytesIO(image_bytes)) as img:
        return np.ascontiguousarray(np.array(img.convert("RGB"), dtype=np.uint8))


def decode_mask(mask_bytes: bytes, *, expected_hw: tuple[int, int]) -> np.ndarray:
    """Decode a strictly L8 mask and binarize it; the size must match the region.

    The wire contract is 8-bit greyscale and nothing else. Accepting RGB/RGBA and
    guessing which channel carries the permission to edit — the alpha, or the
    per-pixel maximum — turns a client bug into an edit of the wrong pixels, so a
    mask of any other mode is refused instead of converted.

    # Raises
    `ValueError` when the image is not mode `L`, when it does not decode to a 2D
    array, or when its size differs from `expected_hw` (height, width).
    """
    import numpy as np
    from PIL import Image

    with Image.open(io.BytesIO(mask_bytes)) as img:
        if img.mode != "L":
            raise ValueError(
                f"Маска должна быть 8-битной в градациях серого (L8), получено «{img.mode}»"
            )
        arr = np.array(img)
    if arr.ndim != 2:
        raise ValueError(f"Некорректная маска: ожидается 2D массив, получено {arr.ndim}D")
    mask = np.ascontiguousarray(arr.astype(np.uint8))
    if tuple(mask.shape[:2]) != tuple(expected_hw):
        raise ValueError(
            f"Размер маски {mask.shape[1]}x{mask.shape[0]} не совпадает с областью "
            f"{expected_hw[1]}x{expected_hw[0]}"
        )
    return np.where(mask > 0, 255, 0).astype(np.uint8)


def require_solid_mask(mask_u8: np.ndarray) -> None:
    """Refuse a `whole_region` request whose mask is not solid.

    `whole_region` does not change the wire format: the client still sends a
    mask, and under this flag it must be filled — every pixel non-zero. Checking
    it turns a client-side disagreement between the flag and the data into an
    immediate, named request error instead of a run whose result silently
    contradicts the flag (the mask would win, and the user would see a partial
    edit while the UI showed "no mask needed").

    `mask_u8` is the already-binarized output of `decode_mask`, so "non-zero"
    and "255" are the same test here.

    # Raises
    `ValueError` naming how many pixels are empty out of how many.
    """
    import numpy as np

    empty = int(np.count_nonzero(mask_u8 == 0))
    if not empty:
        return
    raise ValueError(
        f"Режим «без маски» требует сплошную маску, но {empty} из {mask_u8.size} пикселей "
        "нулевые. Флаг whole_region и присланная маска противоречат друг другу — это ошибка "
        "запроса, а не изображения."
    )


def encode_png_rgb(image_rgb: np.ndarray) -> bytes:
    """Encode an HxWx3 RGB array as PNG bytes."""
    import numpy as np
    from PIL import Image

    arr = np.ascontiguousarray(image_rgb.astype(np.uint8))
    with io.BytesIO() as buf:
        Image.fromarray(arr, "RGB").save(buf, format="PNG")
        return buf.getvalue()
