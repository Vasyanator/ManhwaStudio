"""
File: modules/ai_backend/inpaint/flux2_klein/imaging.py

Purpose:
The pixel work around the diffusion run: decoding the region and the mask off
the wire, the color match and the feathered composite that put the generated
region back over the original, and the mask morphology those need.

The mask here is a PERMISSION TO CHANGE, not a hole to fill: everything outside
it must come back byte-identical, which is why the composite is done here rather
than left to the pipeline's own latent masking.

Main responsibilities:
- wire decoding (`_decode_image_rgb`, `_decode_mask`, `_require_solid_mask`) and
  encoding (`_encode_png_bytes_rgb`);
- color alignment (`_match_color_outside_mask`) and the composite
  (`_composite_over_region`, `_feather_mask_inwards`, `_mask_distance_inside`);
- mask morphology (`_dilate_mask`, `_erode_mask`, `_morph_mask`).

Notes:
numpy is a hard dependency of these helpers; cv2 and PIL are imported lazily
inside the functions that need them, and every cv2 path has a numpy fallback.
"""

from __future__ import annotations

import io
import logging
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    import numpy as np

log = logging.getLogger(__name__)

#: Minimum number of pixels outside the mask required for a meaningful
#: mean/std color match; below it the statistics are noise.
_MIN_COLOR_MATCH_SAMPLES = 256

#: Largest inward distance the cv2-free fallback of `_mask_distance_inside`
#: measures, in pixels. One above the widest feather `normalize_flux2_klein_params`
#: accepts, so the ramp width is never clamped by the probe itself; the fallback
#: costs one erosion per level, which is why it is bounded at all.
MAX_MASK_DISTANCE_PROBE = 33


# =====================================================================
#  Post-processing (ours, not the pipeline's)
# =====================================================================
def _match_color_outside_mask(
    generated: np.ndarray, original: np.ndarray, mask: np.ndarray
) -> np.ndarray:
    """Align the generated region's per-channel mean/std to the original.

    The statistics are taken over the pixels OUTSIDE `mask`, i.e. the ring the
    model was not allowed to change: the VAE round trip shifts the whole window's
    tone, and that ring is the only place where the two images are supposed to be
    identical. Returns `generated` unchanged when the ring is too small to give
    meaningful statistics.
    """
    import numpy as np

    outside = mask == 0
    sample_count = int(np.count_nonzero(outside))
    if sample_count < _MIN_COLOR_MATCH_SAMPLES:
        log.debug(
            "FLUX.2 klein: only %d pixels outside the mask, skipping the color match.",
            sample_count,
        )
        return generated

    reference = original[outside].astype(np.float32)
    produced = generated[outside].astype(np.float32)
    matched = generated.astype(np.float32)
    for channel in range(3):
        ref_mean = float(reference[:, channel].mean())
        ref_std = float(reference[:, channel].std())
        gen_mean = float(produced[:, channel].mean())
        gen_std = float(produced[:, channel].std())
        if gen_std < 1e-3:
            # A flat channel carries no scale to correct; shift it only.
            matched[..., channel] += ref_mean - gen_mean
        else:
            matched[..., channel] = (matched[..., channel] - gen_mean) * (
                ref_std / gen_std
            ) + ref_mean
    return np.clip(matched, 0, 255).astype(np.uint8)


def _composite_over_region(
    original: np.ndarray, generated: np.ndarray, mask: np.ndarray, feather_px: int
) -> np.ndarray:
    """Alpha-blend `generated` into `original` under a feathered `mask`.

    The feather is applied INWARDS (see `_feather_mask_inwards`), so the blend
    weight is exactly zero on every pixel the user did not paint. The final
    `np.where` then guarantees those pixels come back byte-identical, which is
    this service's core contract.

    The blend is ROUNDED, not truncated. Truncation biases every blended pixel
    towards zero by up to one level, and that bias is confined to the mask and to
    nothing else — a faint dark patch in exactly the mask's shape, with a hard
    edge on its contour. Measured on a real page (384x384 region, blob mask,
    `feather_px=6`): blending a region with ITSELF changed 1244 of 30117 masked
    channel values, every one of them one level darker. A coherent one-level step
    along a long contour is far more visible than its magnitude suggests, and it
    was a real part of the seam users reported. Rounding makes the blend an exact
    identity when `generated == original`, which is the property that matters.
    """
    import numpy as np

    inside = mask > 0
    alpha = _feather_mask_inwards(mask, feather_px).astype(np.float32) / 255.0
    alpha = np.where(inside, alpha, 0.0)[..., None]
    blended = original.astype(np.float32) * (1.0 - alpha) + generated.astype(np.float32) * alpha
    blended = np.clip(np.rint(blended), 0, 255).astype(np.uint8)
    return np.ascontiguousarray(np.where(inside[..., None], blended, original))


def _feather_mask_inwards(mask: np.ndarray, feather_px: int) -> np.ndarray:
    """Blend weights that rise from 0 on the mask contour to 1 `feather_px` inside.

    `feather_px` is the RAMP WIDTH in pixels, and the ramp is a smoothstep of the
    distance to the contour, so the weight is exactly zero outside the mask (the
    distance is zero there) and exactly one everywhere at least `feather_px`
    inside it. A mask thinner than `feather_px` compresses the ramp to its own
    half-width instead of losing the edit: the weight still reaches one at the
    mask's core.

    This replaced an erode-then-Gaussian-blur construction whose ramp was neither
    `feather_px` wide nor bounded by it. PIL's `GaussianBlur` takes the radius as
    a standard deviation, so eroding by `f` and blurring by `f` produced a ramp
    about `4*f` wide: measured on a 56 px blob, `f=6` reached weight 1.0 only 22 px
    inside, `f=20` peaked at 0.69 and `f=32` at 0.129 — i.e. asking for a wide
    feather silently discarded up to 87% of the edit, uniformly, across the whole
    mask. It also fell back to a HARD mask whenever the erosion emptied the mask,
    which is the worst possible edge for the case it was meant to protect.
    """
    import numpy as np

    if feather_px <= 0:
        return mask
    distance = _mask_distance_inside(mask)
    reach = float(distance.max())
    if reach <= 0.0:  # pragma: no cover - an empty mask never reaches the blend
        return mask
    # A mask narrower than the requested ramp gets the widest ramp that still
    # reaches full strength somewhere, rather than a partial blend everywhere.
    width = min(float(feather_px), reach)
    t = np.clip(distance / width, 0.0, 1.0)
    smooth = t * t * (3.0 - 2.0 * t)  # smoothstep: zero slope at both ends
    return np.ascontiguousarray((smooth * 255.0 + 0.5).astype(np.uint8))


def _mask_distance_inside(mask: np.ndarray) -> np.ndarray:
    """Distance in pixels from every masked pixel to the nearest unmasked one.

    Zero outside the mask, so a ramp built on it cannot leak past the contour.
    cv2 gives the exact Euclidean distance; without it the distance is built from
    successive erosions, which measures the same thing on the elliptical
    structuring element `_morph_mask` already uses. The fallback costs one
    erosion per level and is bounded by `MAX_MASK_DISTANCE_PROBE`, which is above
    the largest feather the contract accepts.

    **Everything beyond the region's own border counts as unmasked**, which is
    what the one-pixel zero ring below encodes. The region is a WINDOW onto a
    larger page and the pixels past its edge belong to that page, which this
    request may not change — so the region border is as much a mask contour as
    the painted outline is, and the feather has to ramp inwards from it too.
    Without the ring both backends answer "no contour here": `distanceTransform`
    measures only to zeros that exist inside the array, and `cv2.erode` /
    `ImageFilter.MinFilter` extend the border rather than eating into it. A mask
    painted up to the region edge would then meet the untouched page with a hard
    step, and under `whole_region`, where the mask covers everything, the feather
    would do nothing at all.
    """
    import numpy as np

    binary = np.pad((mask > 0).astype(np.uint8), 1)
    try:
        import cv2
    except ImportError:
        pass
    else:
        return np.ascontiguousarray(
            cv2.distanceTransform(binary, cv2.DIST_L2, 5)[1:-1, 1:-1]
        )

    distance = np.zeros(binary.shape, dtype=np.float32)
    # A masked pixel that survives no erosion is one pixel from the outside, so
    # the whole mask starts at 1 and each survived erosion adds another pixel.
    distance[binary > 0] = 1.0
    eroded = binary * 255
    for step in range(2, MAX_MASK_DISTANCE_PROBE + 1):
        eroded = _erode_mask(eroded, 1)
        remaining = eroded > 0
        if not remaining.any():
            break
        distance[remaining] = float(step)
    return np.ascontiguousarray(distance[1:-1, 1:-1])


def _dilate_mask(mask: np.ndarray, radius: int) -> np.ndarray:
    """Grow the mask by `radius` pixels (cv2 when available, PIL otherwise)."""
    return _morph_mask(mask, radius, grow=True)


def _erode_mask(mask: np.ndarray, radius: int) -> np.ndarray:
    """Shrink the mask by `radius` pixels (cv2 when available, PIL otherwise)."""
    return _morph_mask(mask, radius, grow=False)


def _morph_mask(mask: np.ndarray, radius: int, *, grow: bool) -> np.ndarray:
    """Dilate (`grow`) or erode the mask with an elliptical structuring element.

    cv2 is preferred; the PIL fallback exists because OpenCV is an optional
    dependency of this backend. PIL's rank filters cap their window at 31 px, so
    a large radius is applied in several passes.
    """
    if radius <= 0:
        return mask
    kernel_size = 2 * int(radius) + 1
    try:
        import cv2
    except ImportError:
        pass
    else:
        kernel = cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (kernel_size, kernel_size))
        operation = cv2.dilate if grow else cv2.erode
        return operation(mask, kernel, iterations=1)

    import numpy as np
    from PIL import Image, ImageFilter

    rank_filter = ImageFilter.MaxFilter if grow else ImageFilter.MinFilter
    image = Image.fromarray(mask, "L")
    remaining = int(radius)
    while remaining > 0:
        window = min(2 * remaining + 1, 31)
        image = image.filter(rank_filter(window))
        remaining -= (window - 1) // 2
    return np.ascontiguousarray(np.asarray(image, dtype=np.uint8))


# =====================================================================
#  Image / mask codecs
# =====================================================================
def _decode_image_rgb(image_bytes: bytes) -> np.ndarray:
    import numpy as np
    from PIL import Image

    with Image.open(io.BytesIO(image_bytes)) as img:
        return np.ascontiguousarray(np.array(img.convert("RGB"), dtype=np.uint8))


def _decode_mask(mask_bytes: bytes, *, expected_hw: tuple[int, int]) -> np.ndarray:
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


def _require_solid_mask(mask_u8: np.ndarray) -> None:
    """Refuse a `whole_region` request whose mask is not solid.

    `whole_region` does not change the wire format: the client still sends a
    mask, and under this flag it must be filled — every pixel non-zero. Checking
    it turns a client-side disagreement between the flag and the data into an
    immediate, named request error instead of a run whose result silently
    contradicts the flag (the mask would win, and the user would see a partial
    edit while the UI showed "no mask needed").

    `mask_u8` is the already-binarized output of `_decode_mask`, so "non-zero"
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


def _encode_png_bytes_rgb(image_rgb: np.ndarray) -> bytes:
    import numpy as np
    from PIL import Image

    arr = np.ascontiguousarray(image_rgb.astype(np.uint8))
    with io.BytesIO() as buf:
        Image.fromarray(arr, "RGB").save(buf, format="PNG")
        return buf.getvalue()
