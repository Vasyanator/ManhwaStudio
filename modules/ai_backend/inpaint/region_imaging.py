"""
File: modules/ai_backend/inpaint/region_imaging.py

Purpose:
The pixel work around a region editor's diffusion run (FLUX.2 klein, FLUX.3
klein): decoding the region and the mask off the wire, the color match and the
feathered composite that put the generated region back over the original, and
the mask morphology those need. Model-agnostic: numpy arrays in, arrays out.

The mask here is a PERMISSION TO CHANGE, not a hole to fill: everything outside
it must come back byte-identical, which is why the composite is done here rather
than left to the pipeline's own latent masking.

Main responsibilities:
- wire decoding (`decode_image_rgb`, `decode_mask`, `require_solid_mask`) and
  encoding (`encode_png_rgb`);
- color alignment, in run order: the global per-channel mean/std match against
  the ring outside the mask (`match_color_outside_mask`, kept
  `COLOR_MATCH_MARGIN_PX` clear of the contour), then the optional local CIELAB
  match that pulls the low frequencies inside the mask towards the surroundings
  (`match_color_local`: robust local modes — target outside the mask, cascade-
  filled inwards; current inside the mask — lightness mean + contrast, a/b
  mean shift), then the composite (`composite_over_region`,
  `feather_mask_inwards`, `mask_distance_inside`);
- mask morphology (`dilate_mask`, `erode_mask`, `morph_mask`).

Notes:
numpy is a hard dependency of these helpers; cv2 and PIL are imported lazily
inside the functions that need them, and every cv2 path has a numpy fallback
(the local match's Gaussian, box downscale, bilinear upscale and channel mix
included). The local match computes its low-frequency fields on a grid
`scale // LOCAL_MATCH_GRID_SIGMA` times coarser than the region, so its cost is
a few full-resolution passes whatever the scale (2048x2048 with cv2: ~0.7 s at
scale 24, ~1.1 s at scale 8).
"""

from __future__ import annotations

import io
import logging
import math
from typing import TYPE_CHECKING, NamedTuple

if TYPE_CHECKING:
    import numpy as np

log = logging.getLogger(__name__)

#: Minimum number of pixels outside the mask required for a meaningful
#: mean/std color match; below it the statistics are noise.
MIN_COLOR_MATCH_SAMPLES = 256

#: Largest inward distance the cv2-free fallback of `mask_distance_inside`
#: measures, in pixels. One above the widest feather the region editors' wire
#: normalizers accept (32 px), so the ramp width is never clamped by the probe itself; the fallback
#: costs one erosion per level, which is why it is bounded at all.
MAX_MASK_DISTANCE_PROBE = 33

#: Pixels the global color match keeps clear of the mask contour. A text outline
#: or halo the mask does not quite cover sits right at its edge, and including it
#: would bias the fit towards the very text being removed.
COLOR_MATCH_MARGIN_PX = 6

#: Margin, in pixels, between the mask and the pixels the LOCAL match samples
#: its target statistics from (the same reason as `COLOR_MATCH_MARGIN_PX`, a
#: narrower one because the local fit is a spatial interpolation, not one global
#: fit, and a wide margin would push its anchors away from the contour).
LOCAL_MATCH_VALID_MARGIN_PX = 3

#: Number of scales of the local match's cascade fill (`s, 2s, 4s, 8s, 16s`).
LOCAL_MATCH_LEVELS = 5

#: Valid-weight fraction at which one cascade scale is fully trusted.
LOCAL_MATCH_CONFIDENCE = 0.25

#: Range of the local lightness contrast gain (target std / generated std).
LOCAL_MATCH_GAIN_RANGE = (0.8, 1.4)

#: Variance floor of the local statistics, so a flat channel has a finite std.
LOCAL_MATCH_VARIANCE_FLOOR = 1e-3

#: Valid weight below which a cascade level counts as having no support at a
#: cell (its normalized-convolution ratio would be rounding noise).
LOCAL_MATCH_MIN_WEIGHT = 1e-6

#: Similarity scale tau of the local match's robust estimate: a pixel ΔE (CIE76)
#: from the local colour keeps weight `exp(-ΔE² / (2·tau²))` — 0.61 at tau,
#: 0.04 at 2.5·tau. 12 sits between a surface's own texture/noise (a few ΔE,
#: kept) and ink lines, outlines and page background (tens of ΔE, dropped); a
#: generated drift larger than about tau is therefore no longer pulled by the
#: local step (the global step still corrects the VAE's tone shift).
LOCAL_MATCH_SIMILARITY_DE = 12.0

#: Reweighting passes of the robust estimate (mean shift towards the local mode).
LOCAL_MATCH_ROBUST_ITERATIONS = 2

#: Similar-pixel share below which a side's estimate is a mixture rather than a
#: surface: the correction fades linearly to the identity below it.
LOCAL_MATCH_MIN_SHARE = 0.25

#: Gaussian sigma, in grid cells, at which the local match's low-pass works.
#: A larger sigma is applied on a grid downscaled by `floor(sigma / this)`: the
#: fields are smooth by construction, so a few cells per sigma lose nothing
#: visible, and the cascade's 16·s scales stay a fixed, small cost.
LOCAL_MATCH_GRID_SIGMA = 4.0


# =====================================================================
#  Post-processing (ours, not the pipeline's)
# =====================================================================
def match_color_outside_mask(
    generated: np.ndarray, original: np.ndarray, mask: np.ndarray
) -> np.ndarray:
    """Align the generated region's per-channel mean/std to the original.

    The statistics are taken over the pixels outside `mask` grown by
    `COLOR_MATCH_MARGIN_PX`, i.e. the ring the model was not allowed to change:
    the VAE round trip shifts the whole window's tone, and that ring is the only
    place where the two images are supposed to be identical. The margin keeps a
    text outline or halo left right at the mask edge out of the fit. When the
    margin leaves fewer than `MIN_COLOR_MATCH_SAMPLES` pixels the undilated mask
    is used instead; when even that is too few, `generated` is returned unchanged
    (the statistics would be noise).
    """
    import numpy as np

    outside = dilate_mask(mask, COLOR_MATCH_MARGIN_PX) == 0
    sample_count = int(np.count_nonzero(outside))
    if sample_count < MIN_COLOR_MATCH_SAMPLES:
        # A thin ring around a large mask: a margin-free fit beats no fit.
        log.debug(
            "region imaging: only %d pixels outside the mask grown by %d px, "
            "matching the color against the undilated mask instead.",
            sample_count,
            COLOR_MATCH_MARGIN_PX,
        )
        outside = mask == 0
        sample_count = int(np.count_nonzero(outside))
    if sample_count < MIN_COLOR_MATCH_SAMPLES:
        log.debug(
            "region imaging: only %d pixels outside the mask, skipping the color match.",
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
    # Rounded, not truncated: truncation darkens every channel by half a level
    # on average (see `composite_over_region`).
    return np.clip(np.rint(matched), 0, 255).astype(np.uint8)


def match_color_local(
    original: np.ndarray,
    corrected: np.ndarray,
    mask: np.ndarray,
    *,
    strength: float,
    scale_px: int,
) -> np.ndarray:
    """Pull the low frequencies inside `mask` towards the surroundings, in CIELAB.

    Runs after `match_color_outside_mask`, which fixes the VAE's tone shift of the
    whole window but not what the model PAINTED: a diffusion fill tends to average
    the scene's atmosphere, so its colour drifts away from the surroundings
    towards the mask centre and its contrast is flatter. Per pixel, at scale
    `scale_px` (the Gaussian SIGMA of the finest level, in pixels — not a
    window side):

    - lightness: `(L - mu_c) * clip(sd_t / sd_c, LOCAL_MATCH_GAIN_RANGE) + mu_t`;
    - a, b: `a - mu_c + mu_t` (mean shift only: scaling chroma amplifies colour
      noise);
    - the result is `C + (R - C) * strength`.

    `mu_c` / `sd_c` are the statistics of `corrected` taken only from pixels
    INSIDE `mask`, at scale `scale_px`. This deliberately departs from the
    spec's plain Gaussian over all pixels: near the contour that window
    averages the drifted generated pixels with the (already matching) outside,
    sees only part of the drift and inflates `sd_c` across the edge step, so the
    rim stayed under-corrected in a band 1-2 scales wide — a visible seam along
    the contour against the fully corrected centre. `mu_t` / `sd_t` are those of
    `original`, taken only from pixels outside `mask` grown by
    `LOCAL_MATCH_VALID_MARGIN_PX` and carried into the mask by a cascade of
    scales (`scale_px * 2**i`, `LOCAL_MATCH_LEVELS` levels): a single scale has
    no valid pixel in reach at the centre of a wide mask, so each coarser scale
    fills what the finer ones were not confident about, and the global moments
    of the original end the cascade (every Gaussian is truncated at 4 sigma, so
    beyond ~67·`scale_px` from the ring no level has any weight and the target
    converges smoothly to the global one).

    Both sides are ROBUST local modes, not means (`_robust_local_moments`): at
    each scale, `LOCAL_MATCH_ROBUST_ITERATIONS` passes reweight every sample by
    `exp(-ΔE² / (2·LOCAL_MATCH_SIMILARITY_DE²))` against that scale's field at
    the sample's position, and the means and E[L²] are re-estimated with those
    weights. On line art the ring's mean is a darker, greyer fill (ink, white
    outlines, page) and dragged the model's clean fill into a grey patch the
    shape of the mask; the mode keeps the fill. Where a side's similar-pixel
    share falls below `LOCAL_MATCH_MIN_SHARE` the estimate is a mixture, not a
    surface, and the correction fades linearly to the identity there.

    Known limits: (1) a drift larger than about `LOCAL_MATCH_SIMILARITY_DE` is
    no longer pulled by this step (the generated pixels are then "another
    colour"; the global step still corrects the VAE's tone shift). (2) Where a
    surface boundary runs through the mask's interior, each ring sample agrees
    with its own surface, so a coarse window keeps both and the target there
    blends them, pulling one surface towards the other (measured: up to ~8 ΔE
    at strength 0.6, scale 8, as before the robust estimate). The rims next to
    each surface converge to their own surface. The fix would be a
    cross-bilateral estimate (similarity measured at the evaluated point, e.g.
    through colour clusters); deliberately not done.

    The low-frequency fields are evaluated on a coarser grid over a reflect-101
    padded copy of the region (coarse scales on seeded jittered point samples);
    they agree with a full-resolution evaluation to about one level (p99 two
    levels on a region border the mask touches at the largest scale).

    `original`, `corrected`: HxWx3 uint8 RGB of the same shape; `mask`: HxW, non-
    zero = may change. `strength` is clamped to 0..1 (0 returns `corrected`
    itself); a non-finite one is refused. Only pixels inside `mask` are rewritten; every other pixel is
    returned exactly as in `corrected`. The result is ROUNDED to uint8 (see
    `composite_over_region` for why truncation is a visible bias). Returns
    `corrected` unchanged, with a debug log, when the mask is empty or fewer than
    `MIN_COLOR_MATCH_SAMPLES` pixels are valid (the whole region is masked).

    # Raises
    `ValueError` when the shapes disagree, `scale_px` is below 1 or
    `strength` is NaN or infinite.
    """
    import time

    import numpy as np

    if original.shape != corrected.shape or original.ndim != 3 or original.shape[2] != 3:
        raise ValueError(
            f"Локальная подгонка цвета: формы исходника {original.shape} и результата "
            f"{corrected.shape} должны совпадать и быть HxWx3"
        )
    if mask.shape != original.shape[:2]:
        raise ValueError(
            f"Локальная подгонка цвета: маска {mask.shape} не совпадает с областью {original.shape[:2]}"
        )
    if int(scale_px) < 1:
        raise ValueError(f"Локальная подгонка цвета: масштаб должен быть не меньше 1 px, получено {scale_px}")
    strength = float(strength)
    if not math.isfinite(strength):
        raise ValueError(f"Локальная подгонка цвета: сила должна быть конечным числом, получено {strength}")
    strength = min(max(strength, 0.0), 1.0)
    if strength <= 0.0:
        return corrected
    inside = mask > 0
    if not inside.any():
        return corrected
    valid = dilate_mask(mask, LOCAL_MATCH_VALID_MARGIN_PX) == 0
    valid_count = int(np.count_nonzero(valid))
    if valid_count < MIN_COLOR_MATCH_SAMPLES:
        log.debug(
            "region imaging: only %d valid pixels around the mask, skipping the local color match.",
            valid_count,
        )
        return corrected

    started = time.perf_counter()
    scale = float(scale_px)
    height, width = mask.shape
    source_lab = _srgb_u8_to_lab(original)
    corrected_lab = _srgb_u8_to_lab(corrected)

    # Moments are taken about one shared centre near both images' mean, so
    # `E[x²] - E[x]²` does not cancel catastrophically in float32 (L² reaches
    # 1e4). Any value close to the mean conditions it; a strided sample is enough.
    center = corrected_lab[::4, ::4].reshape(-1, 3).mean(axis=0, dtype=np.float64).astype(np.float32)

    # Every low-frequency field lives on one grid `grid_factor` times coarser
    # than the region: the fine pixels' MOMENTS are box-averaged (never the
    # moments of averaged pixels), and the box adds `(f² - 1) / 12` px² of
    # variance that each later sigma subtracts.
    grid_factor = max(1, int(scale // LOCAL_MATCH_GRID_SIGMA))
    box_variance = (grid_factor * grid_factor - 1) / 12.0

    def grid_sigma(sigma_px: float) -> float:
        return float(np.sqrt(max(sigma_px * sigma_px - box_variance, 0.0))) / grid_factor

    # A grid's own reflect-101 border would mirror about the centre of its first
    # CELL, not about the region's first pixel, which biases the statistics by
    # several levels along a border the mask touches at a large scale. So the
    # fine fields are reflect-padded first, by whole cells and by the finest
    # level's kernel reach, and the grid's own border lies outside that reach.
    pad = 0 if grid_factor == 1 else grid_factor * (int(np.ceil(4.0 * grid_sigma(scale))) + 1)
    pad_bottom = pad + (-(height + 2 * pad) % grid_factor)
    pad_right = pad + (-(width + 2 * pad) % grid_factor)
    geometry = _LocalGrid(
        factor=grid_factor,
        crop=(pad, pad + height, pad, pad + width),
        sigmas_px=tuple(scale * float(2**level) for level in range(LOCAL_MATCH_LEVELS)),
    )

    def padded(field: np.ndarray) -> np.ndarray:
        if not (pad or pad_bottom or pad_right):
            return field
        return _reflect_pad(field, pad, pad_bottom, pad, pad_right)

    # Target: the original's dominant local colour outside the margin, carried
    # into the mask by the cascade. Current: the corrected image's dominant
    # local colour over the pixels INSIDE the mask only (see the docstring).
    target, target_share = _robust_local_moments(padded(source_lab - center), padded(valid.astype(np.float32)[..., None])[..., 0], geometry, cascade=True)
    local, local_share = _robust_local_moments(padded(corrected_lab - center), padded(inside.astype(np.float32)[..., None])[..., 0], geometry, cascade=False)
    target_mean = target[..., :3]
    target_std = np.sqrt(np.maximum(target[..., 3] - target_mean[..., 0] ** 2, LOCAL_MATCH_VARIANCE_FLOOR))
    local_mean = local[..., :3]
    local_std = np.sqrt(np.maximum(local[..., 3] - local_mean[..., 0] ** 2, LOCAL_MATCH_VARIANCE_FLOOR))

    # The correction is linear per pixel: L' = L·gain + offset_L, a' = a + shift_a,
    # b' = b + shift_b, on absolute Lab values (the shared centre is folded in).
    # Where either side's dominant colour is carried by too few similar pixels
    # the estimate is not a surface but a mixture, so the correction fades to
    # the identity (gain 1, no offset) instead of pulling towards it.
    trust = np.clip(np.minimum(target_share, local_share) / LOCAL_MATCH_MIN_SHARE, 0.0, 1.0)
    gain = 1.0 + (np.clip(target_std / local_std, *LOCAL_MATCH_GAIN_RANGE) - 1.0) * trust
    shift = (target_mean - local_mean) * trust[..., None]
    offset_l = (target_mean[..., 0] + center[0]) * trust - (local_mean[..., 0] + center[0]) * (gain - 1.0 + trust)
    fields = np.stack((gain, offset_l, shift[..., 1], shift[..., 2]), axis=2).astype(np.float32)
    fields = _upsample_linear(fields, grid_factor, padded(np.zeros((height, width, 1), dtype=np.float32)).shape[:2])[pad : pad + height, pad : pad + width]

    at = fields[inside]
    lab = corrected_lab[inside]
    remapped = np.empty_like(lab)
    remapped[:, 0] = lab[:, 0] * at[:, 0] + at[:, 1]
    remapped[:, 1] = lab[:, 1] + at[:, 2]
    remapped[:, 2] = lab[:, 2] + at[:, 3]
    blended = lab + (remapped - lab) * np.float32(strength)
    out = corrected.copy()
    out[inside] = _lab_to_srgb_u8(blended)
    log.debug(
        "region imaging: local color match %dx%d, scale %d px, strength %.2f, grid /%d, %.1f ms.",
        width,
        height,
        int(scale_px),
        strength,
        grid_factor,
        (time.perf_counter() - started) * 1000.0,
    )
    return np.ascontiguousarray(out)


class _LocalGrid(NamedTuple):
    """Geometry shared by both sides of the local match.

    `factor`: fine pixels per grid cell; `crop`: (top, bottom, left, right) of
    the region inside the padded fine fields (`top` / `left` are whole cells);
    `sigmas_px`: the cascade's Gaussian sigmas in fine pixels, finest first (the
    current side uses the first only).
    """

    factor: int
    crop: tuple[int, int, int, int]
    sigmas_px: tuple[float, ...]


def _jittered_sample_indices(height: int, width: int, stride: int, seed: int) -> tuple[np.ndarray, np.ndarray]:
    """Row and column index grids picking ONE pixel per `stride` x `stride` block, at a seeded random offset.

    Point samples keep every sample a real pixel (an ink line stays an ink
    sample, not an ink-and-fill blend), and the per-block jitter keeps a
    periodic pattern (screentone, hatching) whose period divides the stride
    from aliasing into a biased sample. Seeded, so a run is reproducible.
    """
    import numpy as np

    rows = -(-height // stride)
    cols = -(-width // stride)
    rng = np.random.default_rng(seed)
    row_index = np.minimum(np.arange(rows)[:, None] * stride + rng.integers(0, stride, size=(rows, cols)), height - 1)
    col_index = np.minimum(np.arange(cols)[None, :] * stride + rng.integers(0, stride, size=(rows, cols)), width - 1)
    return row_index, col_index


def _robust_local_moments(
    centred_lab: np.ndarray, base_weight: np.ndarray, geometry: _LocalGrid, *, cascade: bool
) -> tuple[np.ndarray, np.ndarray]:
    """Dominant local colour [L, a, b, E[L²]] (centred) on the grid, and its share.

    `centred_lab`: padded HxWx3 fine Lab minus the shared centre; `base_weight`:
    padded HxW 0/1 weight of the pixels this side may sample. Each scale starts
    from its weighted mean field, then `LOCAL_MATCH_ROBUST_ITERATIONS` times
    reweights every sample by `exp(-ΔE² / (2·LOCAL_MATCH_SIMILARITY_DE²))`
    against that scale's field at the sample's own position (mean shift towards
    the local mode at that scale) and re-estimates: on line art the ink,
    outlines and page drop out and the fill survives, and the same weights give
    the L variance, so ink does not inflate the gain. Reweighting per SCALE
    matters: a surface that dominates its own small neighbourhood (a white page
    beyond the panel border) is a minority at a coarse scale and must not leak
    into the coarse fill of the mask's interior with full weight.

    Scale `i` (sigma `sigmas_px[i]`) works on jittered point samples at stride
    `2**i` (`_jittered_sample_indices`) and its own grid `factor·2**i` px a
    cell, so every scale costs a quarter of the previous one and the whole
    cascade about 1.3 full-resolution passes per reweighting.

    `cascade=True` (the target) fills the field into regions without samples
    through every scale and ends with the robust global moments (weighted by the
    coarsest scale's similarities); `cascade=False` (the current side) uses the
    finest scale only, with the robust global moments where no sample is in
    reach. The share is the similarity-weighted fraction of the sampled pixels
    (1 = all agree with the field), filled the same way; the caller fades the
    correction to the identity where it is low.
    """
    import numpy as np

    top, bottom, left, right = geometry.crop
    factor = geometry.factor
    grid_shape = (base_weight.shape[0] // factor, base_weight.shape[1] // factor)

    def robust_scale(level: int) -> tuple[np.ndarray, np.ndarray, np.ndarray, np.ndarray, np.float32]:
        stride = 2**level
        if stride == 1:
            lab, base = centred_lab, base_weight
        else:
            row_index, col_index = _jittered_sample_indices(base_weight.shape[0], base_weight.shape[1], stride, seed=level)
            lab, base = centred_lab[row_index, col_index], base_weight[row_index, col_index]
        cell_px = factor * stride
        sigma_px = geometry.sigmas_px[level]
        sigma = float(np.sqrt(max(sigma_px * sigma_px - (cell_px * cell_px - 1) / 12.0, 0.0))) / cell_px
        base_grid = _box_downsample(base[..., None], factor)
        # The region's own cells (a ragged edge cell holds a few reflected
        # pixels, negligible for a global fallback).
        cells = (slice(top // cell_px, -(-bottom // cell_px)), slice(left // cell_px, -(-right // cell_px)))
        base_sum = float(base_grid[cells].sum(dtype=np.float64))

        def estimate(robust: np.ndarray) -> tuple[np.ndarray, np.ndarray, np.ndarray, np.ndarray, np.float32]:
            # Normalized convolution of [robust·Lab, robust·L²] by robust:
            # value, share and spatial confidence; a cell with no (similar)
            # sample reads as the robust global estimate.
            weighted = lab * robust[..., None]
            grid = np.concatenate(
                (
                    base_grid,
                    _box_downsample(robust[..., None], factor),
                    _box_downsample(weighted, factor),
                    _box_downsample(lab[..., :1] * weighted[..., :1], factor),
                ),
                axis=2,
            )
            del weighted
            sums = grid[cells].reshape(-1, grid.shape[2]).sum(axis=0, dtype=np.float64)
            robust_sum = float(sums[1])
            totals = (sums[2:] / robust_sum).astype(np.float32) if robust_sum > 0.0 else np.zeros(4, dtype=np.float32)
            share_total = np.float32(robust_sum / base_sum) if base_sum > 0.0 else np.float32(0.0)
            blurred = _smooth(grid, sigma)
            cell_base, cell_weight = blurred[..., :1], blurred[..., 1:2]
            value = np.where(cell_weight > LOCAL_MATCH_MIN_WEIGHT, blurred[..., 2:] / np.maximum(cell_weight, LOCAL_MATCH_MIN_WEIGHT), totals)
            share = np.where(cell_base > LOCAL_MATCH_MIN_WEIGHT, cell_weight / np.maximum(cell_base, LOCAL_MATCH_MIN_WEIGHT), share_total)
            confidence = np.clip(cell_base / LOCAL_MATCH_CONFIDENCE, 0.0, 1.0)
            return value, share, confidence, totals, share_total

        value, share, confidence, totals, share_total = estimate(base)
        for _ in range(LOCAL_MATCH_ROBUST_ITERATIONS):
            mean_at_samples = _upsample_linear(np.ascontiguousarray(value[..., :3]), factor, base.shape)
            distance = lab - mean_at_samples
            # einsum: numpy's reduce over a length-3 last axis is ~10x slower.
            similarity = np.exp(np.einsum("ijk,ijk->ij", distance, distance) * np.float32(-0.5 / LOCAL_MATCH_SIMILARITY_DE**2))
            del mean_at_samples, distance
            value, share, confidence, totals, share_total = estimate(base * similarity.astype(np.float32))
        if stride > 1:
            # This scale's cells span exactly `stride` x `stride` grid cells.
            stacked = _upsample_linear(np.concatenate((value, share, confidence), axis=2), stride, grid_shape)
            value, share, confidence = stacked[..., :4], stacked[..., 4:5], stacked[..., 5:6]
        return value, share[..., 0], confidence[..., 0], totals, share_total

    moments, share, confidence, totals, share_total = robust_scale(0)
    if not cascade:
        return moments, share
    for level in range(1, len(geometry.sigmas_px)):
        level_value, level_share, level_confidence, totals, share_total = robust_scale(level)
        moments = confidence[..., None] * moments + (1.0 - confidence[..., None]) * level_value
        share = confidence * share + (1.0 - confidence) * level_share
        confidence = np.maximum(confidence, level_confidence)
    moments = confidence[..., None] * moments + (1.0 - confidence[..., None]) * totals
    share = confidence * share + (1.0 - confidence) * share_total
    return moments, share

# ---------------------------------------------------------------------
#  CIELAB (sRGB, D65) and the local match's low-pass
# ---------------------------------------------------------------------
#: D65 reference white of the XYZ <-> Lab conversion.
_D65_WHITE = (0.95047, 1.0, 1.08883)

#: Linear sRGB -> XYZ (D65), rows X, Y, Z.
_SRGB_TO_XYZ = (
    (0.4124564, 0.3575761, 0.1804375),
    (0.2126729, 0.7151522, 0.0721750),
    (0.0193339, 0.1191920, 0.9503041),
)

#: XYZ (D65) -> linear sRGB, the inverse of `_SRGB_TO_XYZ`.
_XYZ_TO_SRGB = (
    (3.2404542, -1.5371385, -0.4985314),
    (-0.9692660, 1.8760108, 0.0415560),
    (0.0556434, -0.2040259, 1.0572252),
)

#: CIELAB's linear-segment threshold `(6/29)`.
_LAB_DELTA = 6.0 / 29.0


def _srgb_u8_to_lab(rgb: np.ndarray) -> np.ndarray:
    """HxWx3 uint8 sRGB -> HxWx3 float32 CIELAB (D65): L in 0..100."""
    import numpy as np

    levels = np.arange(256, dtype=np.float64) / 255.0
    to_linear = np.where(levels <= 0.04045, levels / 12.92, ((levels + 0.055) / 1.055) ** 2.4).astype(np.float32)
    linear = to_linear[rgb]
    matrix = np.asarray(_SRGB_TO_XYZ, dtype=np.float32) / np.asarray(_D65_WHITE, dtype=np.float32)[:, None]
    xyz = _mix_channels(linear, matrix)
    f = np.where(
        xyz > _LAB_DELTA**3,
        np.cbrt(np.maximum(xyz, 0.0)),
        xyz / (3.0 * _LAB_DELTA * _LAB_DELTA) + 4.0 / 29.0,
    ).astype(np.float32)
    lab = np.empty_like(f)
    lab[..., 0] = 116.0 * f[..., 1] - 16.0
    lab[..., 1] = 500.0 * (f[..., 0] - f[..., 1])
    lab[..., 2] = 200.0 * (f[..., 1] - f[..., 2])
    return lab


def _lab_to_srgb_u8(lab: np.ndarray) -> np.ndarray:
    """...x3 float CIELAB (D65) -> ...x3 uint8 sRGB, clipped to the gamut and ROUNDED."""
    import numpy as np

    fy = (lab[..., 0] + 16.0) / 116.0
    f = np.stack((fy + lab[..., 1] / 500.0, fy, fy - lab[..., 2] / 200.0), axis=-1)
    xyz = np.where(f > _LAB_DELTA, f * f * f, 3.0 * _LAB_DELTA * _LAB_DELTA * (f - 4.0 / 29.0))
    xyz = xyz * np.asarray(_D65_WHITE, dtype=np.float32)
    linear = np.clip(_mix_channels(xyz.astype(np.float32), np.asarray(_XYZ_TO_SRGB, dtype=np.float32)), 0.0, 1.0)
    encoded = np.where(linear <= 0.0031308, 12.92 * linear, 1.055 * np.power(linear, 1.0 / 2.4) - 0.055)
    return np.clip(np.rint(encoded * 255.0), 0, 255).astype(np.uint8)


def _mix_channels(field: np.ndarray, matrix: np.ndarray) -> np.ndarray:
    """`out[..., i] = sum_j matrix[i, j] * field[..., j]` for a ...x3 float32 field and a 3x3 matrix.

    cv2.transform when available; the numpy fallback is three explicit sums,
    because numpy's `(N, 3) @ (3, 3)` is a poor BLAS shape (~20x slower).
    """
    import numpy as np

    try:
        import cv2
    except ImportError:
        pass
    else:
        flat = np.ascontiguousarray(field, dtype=np.float32).reshape(-1, 1, 3)
        return cv2.transform(flat, np.asarray(matrix, dtype=np.float32)).reshape(field.shape)
    out = np.empty(field.shape, dtype=np.float32)
    for row in range(3):
        out[..., row] = field[..., 0] * matrix[row, 0] + field[..., 1] * matrix[row, 1] + field[..., 2] * matrix[row, 2]
    return out


def _smooth(field: np.ndarray, sigma: float) -> np.ndarray:
    """Gaussian low-pass of an HxWxK float32 field with standard deviation `sigma` (cells).

    A sigma of at least `2 * LOCAL_MATCH_GRID_SIGMA` is applied on a grid
    `floor(sigma / LOCAL_MATCH_GRID_SIGMA)` times coarser (box down, blur, linear
    up), so the cost does not grow with sigma; the result is smooth on the scale
    of the coarse cells, which is what makes the interpolation lossless enough.
    """
    import numpy as np

    if sigma <= 0.0:
        return field
    factor = int(sigma // LOCAL_MATCH_GRID_SIGMA)
    if factor < 2:
        return _gaussian_blur(field, sigma)
    coarse = _box_downsample(field, factor)
    coarse_sigma = float(np.sqrt(max(sigma * sigma - (factor * factor - 1) / 12.0, 0.0))) / factor
    return _upsample_linear(_gaussian_blur(coarse, coarse_sigma), factor, field.shape[:2])


def _box_downsample(field: np.ndarray, factor: int) -> np.ndarray:
    """Mean over `factor` x `factor` blocks of an HxWxK field; the ragged edge is padded by replication."""
    import numpy as np

    if factor <= 1:
        return np.ascontiguousarray(field, dtype=np.float32)
    height, width, channels = field.shape
    pad_h = -height % factor
    pad_w = -width % factor
    if pad_h or pad_w:
        field = np.pad(field, ((0, pad_h), (0, pad_w), (0, 0)), mode="edge")
    coarse_h = (height + pad_h) // factor
    coarse_w = (width + pad_w) // factor
    try:
        import cv2
    except ImportError:
        pass
    else:
        if channels <= 4:
            # INTER_AREA at an exact integer ratio IS the block mean, ~20x faster.
            area = cv2.resize(np.ascontiguousarray(field, dtype=np.float32), (coarse_w, coarse_h), interpolation=cv2.INTER_AREA)
            return np.ascontiguousarray(area.reshape(coarse_h, coarse_w, channels))
    blocks = field.reshape(coarse_h, factor, coarse_w, factor, channels)
    return np.ascontiguousarray(blocks.mean(axis=(1, 3), dtype=np.float32))


def _upsample_linear(field: np.ndarray, factor: int, shape_hw: tuple[int, int]) -> np.ndarray:
    """Bilinear inverse of `_box_downsample`: an hxwxK grid back to `shape_hw`.

    Coarse cell `i` is centred on fine coordinate `i·factor + (factor - 1) / 2`
    (the alignment of `cv2.resize(INTER_LINEAR)` to `(w·factor, h·factor)`, whose
    output is then cropped to `shape_hw`); coordinates past the outer centres
    clamp to the edge cell.
    """
    import numpy as np

    height, width = shape_hw
    if factor <= 1:
        return np.ascontiguousarray(field[:height, :width])
    coarse_h, coarse_w, channels = field.shape
    try:
        import cv2
    except ImportError:
        pass
    else:
        resized = cv2.resize(field, (coarse_w * factor, coarse_h * factor), interpolation=cv2.INTER_LINEAR)
        return np.ascontiguousarray(resized.reshape(coarse_h * factor, coarse_w * factor, channels)[:height, :width])

    def axis_weights(fine: int, coarse: int) -> tuple[np.ndarray, np.ndarray, np.ndarray]:
        position = (np.arange(fine, dtype=np.float32) + 0.5) / factor - 0.5
        position = np.clip(position, 0.0, float(coarse - 1))
        low = np.floor(position).astype(np.intp)
        high = np.minimum(low + 1, coarse - 1)
        return low, high, (position - low).astype(np.float32)

    row_low, row_high, row_t = axis_weights(height, coarse_h)
    col_low, col_high, col_t = axis_weights(width, coarse_w)
    rows = field[row_low] * (1.0 - row_t)[:, None, None] + field[row_high] * row_t[:, None, None]
    out = rows[:, col_low] * (1.0 - col_t)[None, :, None] + rows[:, col_high] * col_t[None, :, None]
    return np.ascontiguousarray(out, dtype=np.float32)


def _gaussian_blur(field: np.ndarray, sigma: float) -> np.ndarray:
    """Separable Gaussian blur of an HxWxK float32 field, reflect-101 border.

    cv2 when available; the numpy fallback builds the kernel cv2 builds for a
    float image with `ksize=(0, 0)` (`round(8·sigma + 1) | 1` taps) and the same
    border, so both paths agree to float rounding.
    """
    import numpy as np

    if sigma <= 0.0:
        return field
    field = np.ascontiguousarray(field, dtype=np.float32)
    try:
        import cv2
    except ImportError:
        pass
    else:
        blurred = cv2.GaussianBlur(field, (0, 0), sigmaX=sigma, sigmaY=sigma, borderType=cv2.BORDER_REFLECT_101)
        return blurred.reshape(field.shape)

    taps = int(round(8.0 * sigma + 1.0)) | 1
    radius = taps // 2
    offsets = np.arange(-radius, radius + 1, dtype=np.float64)
    kernel = np.exp(-(offsets * offsets) / (2.0 * sigma * sigma))
    kernel = (kernel / kernel.sum()).astype(np.float32)
    out = field
    for axis in (0, 1):
        length = out.shape[axis]
        index = _reflect101_indices(np.arange(-radius, length + radius), length)
        padded = np.take(out, index, axis=axis)
        accumulated = np.zeros_like(out)
        for tap, weight in enumerate(kernel):
            window = padded[tap : tap + length] if axis == 0 else padded[:, tap : tap + length]
            accumulated += weight * window
        out = accumulated
    return out


def _reflect_pad(field: np.ndarray, top: int, bottom: int, left: int, right: int) -> np.ndarray:
    """Reflect-101 pad an HxWxK field (`cv2.BORDER_REFLECT_101`), at any width — even wider than the field."""
    import numpy as np

    height, width = field.shape[:2]
    rows = _reflect101_indices(np.arange(-top, height + bottom), height)
    cols = _reflect101_indices(np.arange(-left, width + right), width)
    return np.ascontiguousarray(np.take(np.take(field, rows, axis=0), cols, axis=1))


def _reflect101_indices(index: np.ndarray, length: int) -> np.ndarray:
    """Map any integer index onto 0..length-1 by reflect-101 (`cv2.BORDER_REFLECT_101`), at any distance."""
    import numpy as np

    if length == 1:
        return np.zeros_like(index)
    period = 2 * (length - 1)
    folded = np.abs(index) % period
    return np.where(folded >= length, period - folded, folded)


def composite_over_region(
    original: np.ndarray, generated: np.ndarray, mask: np.ndarray, feather_px: int
) -> np.ndarray:
    """Alpha-blend `generated` into `original` under a feathered `mask`.

    The feather is applied INWARDS (see `feather_mask_inwards`), so the blend
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
    alpha = feather_mask_inwards(mask, feather_px).astype(np.float32) / 255.0
    alpha = np.where(inside, alpha, 0.0)[..., None]
    blended = original.astype(np.float32) * (1.0 - alpha) + generated.astype(np.float32) * alpha
    blended = np.clip(np.rint(blended), 0, 255).astype(np.uint8)
    return np.ascontiguousarray(np.where(inside[..., None], blended, original))


def feather_mask_inwards(mask: np.ndarray, feather_px: int) -> np.ndarray:
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
    distance = mask_distance_inside(mask)
    reach = float(distance.max())
    if reach <= 0.0:  # pragma: no cover - an empty mask never reaches the blend
        return mask
    # A mask narrower than the requested ramp gets the widest ramp that still
    # reaches full strength somewhere, rather than a partial blend everywhere.
    width = min(float(feather_px), reach)
    t = np.clip(distance / width, 0.0, 1.0)
    smooth = t * t * (3.0 - 2.0 * t)  # smoothstep: zero slope at both ends
    return np.ascontiguousarray((smooth * 255.0 + 0.5).astype(np.uint8))


def mask_distance_inside(mask: np.ndarray) -> np.ndarray:
    """Distance in pixels from every masked pixel to the nearest unmasked one.

    Zero outside the mask, so a ramp built on it cannot leak past the contour.
    cv2 gives the exact Euclidean distance; without it the distance is built from
    successive erosions, which measures the same thing on the elliptical
    structuring element `morph_mask` already uses. The fallback costs one
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
        eroded = erode_mask(eroded, 1)
        remaining = eroded > 0
        if not remaining.any():
            break
        distance[remaining] = float(step)
    return np.ascontiguousarray(distance[1:-1, 1:-1])


def dilate_mask(mask: np.ndarray, radius: int) -> np.ndarray:
    """Grow the mask by `radius` pixels (cv2 when available, PIL otherwise)."""
    return morph_mask(mask, radius, grow=True)


def erode_mask(mask: np.ndarray, radius: int) -> np.ndarray:
    """Shrink the mask by `radius` pixels (cv2 when available, PIL otherwise)."""
    return morph_mask(mask, radius, grow=False)


def morph_mask(mask: np.ndarray, radius: int, *, grow: bool) -> np.ndarray:
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
