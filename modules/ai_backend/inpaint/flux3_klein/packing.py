"""
File: modules/ai_backend/inpaint/flux3_klein/packing.py

Purpose:
Everything between PIL images and the DiT's token pack: reference-image
preparation (single-reference canvas, multi-reference grid), image <-> tensor
conversion, rope-id scaling, reference token assembly, the initial noise, and
the mode-major `Packed` the DiT and the sampler share.

Main responsibilities:
- `Packed` / `MODES` - the DiT input contract (WP3's `dit.py` imports them);
- `pack` - samples of `(tokens, ids)` pairs -> `Packed`;
- reference preparation: `reference_canvas`, `prepare_reference`,
  `prepare_multi_reference`, `prepare_references` (-> `PreparedReferences`),
  `default_size`;
- `to_tensor` / `to_pil`;
- `scaled_ids`, `reference_tokens`, `initial_noise`.

Key declarations:
- `MODES`, `Packed`, `PreparedReferences`
- `MATCH_MAX_PIXELS`, `MULTI_REFERENCE_MAX_PIXELS`

Notes:
- Ported from black-forest-labs/flux-3-image-klein `src/flux3/pipeline.py`,
  `src/flux3/generate.py` and `src/flux3/dit.py` (commit e53f4ea), Copyright
  2026 Black Forest Labs, Inc., licensed under the Apache License, Version 2.0
  (see `LICENSE` in this directory).
- Modified: `Packed` and `MODES` moved here from `dit.py`, so the sampler and
  the DiT share them without the sampler importing the DiT; `pack` takes the
  per-mode channel counts from the caller (`DitConfig.in_channels`) instead of
  module constants and validates every `(tokens, ids)` pair; `scaled_ids` takes
  the rope scale (`DitConfig.image_rope_scale`) and no longer scales its input
  in place when it already is float32; the reference selection of `generate.py`
  is `prepare_references`; the per-reference id/token assembly of
  `Pipeline.encode_references` is `reference_tokens` (the AE call stays with
  the caller); the noise draw of `Pipeline.generate` is `initial_noise`;
  `default_size` takes the text-to-image size from the caller; side bounds come
  from `params.py`; the reference pixel caps can be LOWERED by the caller
  (`Flux3Options.reference_max_pixels`, a low-VRAM option absent from the
  reference; `None` keeps the reference caps exactly).
- Torch, numpy and PIL are imported at module level: this module is only
  imported by the torch-side components.
"""

from __future__ import annotations

import math
from collections.abc import Mapping, Sequence
from dataclasses import dataclass

import numpy as np
import torch
from PIL import Image, ImageOps
from torch import Tensor

from .params import MAX_SIDE, MIN_REFERENCE_PIXELS, MIN_SIDE, SIDE_MULTIPLE

#: The DiT input modes, sorted; also the order tokens take inside one sample
#: in the single-stream blocks (reference `dit.py` `MODES`).
MODES: tuple[str, ...] = ("image", "image_cond", "text")

#: Pixel cap of a single reference's canvas, padding included (reference
#: `pipeline.py` `MATCH_MAX_PIXELS`).
MATCH_MAX_PIXELS = 4 * 1024 * 1024

#: Pixel cap of each reference of a multi-reference edit (reference
#: `MULTI_REFERENCE_MAX_PIXELS`).
MULTI_REFERENCE_MAX_PIXELS = 1024 * 1024

#: `(tokens, ids)` of one mode of one sample: `tokens [N, C]`, `ids [N, 4]`.
TokenPair = tuple[Tensor, Tensor]

#: A `(left, top, right, bottom)` pixel box.
Box = tuple[int, int, int, int]


@dataclass
class Packed:
    """Mode-major token pack of one or more samples.

    `data[m]` is `[N_m, in_channels[m]]`, `ids[m]` is `[N_m, 4]` float32 rope ids
    `(t, h, w, l)`, `timesteps[m]` is `[N_m]` bf16, and
    `mode_indices[m][s] = (start, end)` is sample `s`'s span in `data[m]` (an
    empty span for a mode the sample lacks). Every mode has one span per sample.
    `data` keeps the dtype of the tokens given to `pack` (the image noise stays
    float32, as in the reference); an empty mode is bf16. The sampler replaces
    `data["image"]` and fills `timesteps["image"]` in place between steps.
    """

    data: dict[str, Tensor]
    ids: dict[str, Tensor]
    timesteps: dict[str, Tensor]
    mode_indices: dict[str, list[tuple[int, int]]]

    @property
    def num_samples(self) -> int:
        """Number of samples in the pack (the span count of the `image` mode)."""
        return len(self.mode_indices["image"])


@dataclass(frozen=True)
class PreparedReferences:
    """References ready for the AE (and, in VL mode, the text encoder).

    `crop_box` is set only for one padded reference when the caller gave no
    output size: the generated canvas is then cropped back to this box.
    """

    images: tuple[Image.Image, ...]
    crop_box: Box | None


def _check_pair(mode: str, item: TokenPair, channels: int) -> None:
    """Refuse a `(tokens, ids)` pair whose shapes do not fit `mode`."""
    tokens, ids = item
    if tokens.ndim != 2 or tokens.shape[1] != channels:
        raise ValueError(f"pack: токены режима «{mode}» имеют форму {tuple(tokens.shape)}, ожидается [N, {channels}]")
    if ids.ndim != 2 or ids.shape[1] != 4 or ids.shape[0] != tokens.shape[0]:
        raise ValueError(
            f"pack: id режима «{mode}» имеют форму {tuple(ids.shape)}, ожидается [{tokens.shape[0]}, 4]"
        )


def pack(samples: Sequence[tuple[TokenPair, TokenPair | None, TokenPair]], in_channels: Mapping[str, int]) -> Packed:
    """Pack `samples[s] = (text, image_cond or None, image)` into one `Packed`.

    `in_channels` maps every mode of `MODES` to its token width (the DiT's
    `in_channels`); it sizes the empty tensor of a mode no sample has. The device
    is the first sample's text tokens' device. Timesteps start at zero (bf16).

    # Raises
    `ValueError` for no samples, a mode missing from `in_channels`, or a pair
    whose shapes do not match its mode.
    """
    if not samples:
        raise ValueError("pack: нет ни одного образца")
    missing = [mode for mode in MODES if mode not in in_channels]
    if missing:
        raise ValueError(f"pack: в in_channels нет режимов {missing}")
    data: dict[str, list[Tensor]] = {m: [] for m in MODES}
    ids: dict[str, list[Tensor]] = {m: [] for m in MODES}
    mode_indices: dict[str, list[tuple[int, int]]] = {m: [] for m in MODES}
    counts = dict.fromkeys(MODES, 0)
    for text, cond, image in samples:
        for mode, item in (("text", text), ("image_cond", cond), ("image", image)):
            start = counts[mode]
            if item is not None:
                _check_pair(mode, item, in_channels[mode])
                data[mode].append(item[0])
                ids[mode].append(item[1])
                counts[mode] += item[0].shape[0]
            mode_indices[mode].append((start, counts[mode]))
    device = samples[0][0][0].device

    def cat(parts: list[Tensor], width: int, dtype: torch.dtype) -> Tensor:
        """`torch.cat(parts)`, or an empty `[0, width]` tensor of `dtype`."""
        return torch.cat(parts) if parts else torch.zeros((0, width), device=device, dtype=dtype)

    packed_data = {m: cat(data[m], in_channels[m], torch.bfloat16) for m in MODES}
    # The ids are always float32: the image ids are scaled by a fraction.
    packed_ids = {m: cat(ids[m], 4, torch.float32).float() for m in MODES}
    return Packed(
        data=packed_data,
        ids=packed_ids,
        timesteps={m: torch.zeros(packed_data[m].shape[0], device=device, dtype=torch.bfloat16) for m in MODES},
        mode_indices=mode_indices,
    )


def scaled_ids(ids: Tensor, rope_scale: float) -> Tensor:
    """A float32 copy of latent `ids` with the h and w axes multiplied by `rope_scale`."""
    scaled = ids.to(torch.float32, copy=True)
    scaled[:, 1:3] *= rope_scale
    return scaled


def _ceil_to_grid(value: int) -> int:
    """`value` rounded up to a multiple of `SIDE_MULTIPLE`."""
    return -(-value // SIDE_MULTIPLE) * SIDE_MULTIPLE


def _pixel_cap(reference_cap: int, max_pixels: int | None) -> int:
    """`reference_cap`, lowered to `max_pixels` when that is given and smaller.

    # Raises
    `ValueError` for a `max_pixels` below `MIN_REFERENCE_PIXELS` (the smallest
    canvas the side bounds allow).
    """
    if max_pixels is None:
        return reference_cap
    if max_pixels < MIN_REFERENCE_PIXELS:
        raise ValueError(f"Предел размера референса {max_pixels} пикселей меньше допустимого {MIN_REFERENCE_PIXELS}")
    return min(reference_cap, max_pixels)


def reference_canvas(
    width: int, height: int, *, max_pixels: int | None = None
) -> tuple[tuple[int, int], tuple[int, int]]:
    """`(fitted size, canvas size)` of a single `width` x `height` reference.

    The fitted size keeps the aspect, is never upscaled, and keeps the canvas
    (the fitted size rounded up to the 16 grid, sides clamped to 64..4096) at
    most `MATCH_MAX_PIXELS`, or `max_pixels` when that is smaller. Both are
    `(width, height)`.

    # Raises
    `ValueError` for a non-positive side or a `max_pixels` below
    `MIN_REFERENCE_PIXELS`.
    """
    if width <= 0 or height <= 0:
        raise ValueError(f"reference_canvas: размер {width}x{height} должен быть положительным")
    cap = _pixel_cap(MATCH_MAX_PIXELS, max_pixels)
    scale = min(1.0, math.sqrt(cap / (width * height)), MAX_SIDE / max(width, height))
    # The rounding up to the grid can push the canvas over the cap; shrink by
    # 0.5 % until it fits (reference loop, terminates: the scale decreases
    # geometrically and a 64x64 canvas always fits, `cap >= MIN_REFERENCE_PIXELS`).
    while True:
        fit = (max(1, round(width * scale)), max(1, round(height * scale)))
        canvas = (min(MAX_SIDE, max(MIN_SIDE, _ceil_to_grid(fit[0]))), min(MAX_SIDE, max(MIN_SIDE, _ceil_to_grid(fit[1]))))
        if canvas[0] * canvas[1] <= cap:
            return fit, canvas
        scale *= 0.995


def prepare_reference(image: Image.Image, *, max_pixels: int | None = None) -> tuple[Image.Image, Box | None]:
    """A single reference: EXIF-transposed RGB, fitted by `reference_canvas`
    (with `max_pixels`) and padded, centred, to its canvas.

    Returns the image and, when it was padded, the `(left, top, right, bottom)`
    box of the fitted image inside the canvas (else `None`). The padding
    reflects the image; an image whose short side is at most 16 px is padded
    symmetrically instead (the reference rule).
    """
    image = ImageOps.exif_transpose(image).convert("RGB")
    fit, (width, height) = reference_canvas(image.width, image.height, max_pixels=max_pixels)
    if fit != image.size:
        image = image.resize(fit, Image.Resampling.LANCZOS)
    if (width, height) == image.size:
        return image, None
    left, top = (width - image.width) // 2, (height - image.height) // 2
    padding = ((top, height - image.height - top), (left, width - image.width - left), (0, 0))
    mode = "reflect" if min(image.size) > SIDE_MULTIPLE else "symmetric"
    padded = Image.fromarray(np.pad(np.asarray(image), padding, mode=mode))
    return padded, (left, top, left + image.width, top + image.height)


def prepare_multi_reference(image: Image.Image, *, max_pixels: int | None = None) -> Image.Image:
    """One reference of a multi-reference edit: EXIF-transposed RGB, downscaled
    under `MULTI_REFERENCE_MAX_PIXELS` (or `max_pixels` when smaller; sides
    floored), then centre-cropped to the 16 grid.

    # Raises
    `ValueError` when the result would have a side below 16 px, or for a
    `max_pixels` below `MIN_REFERENCE_PIXELS`.
    """
    cap = _pixel_cap(MULTI_REFERENCE_MAX_PIXELS, max_pixels)
    image = ImageOps.exif_transpose(image).convert("RGB")
    width, height = image.size
    if width * height > cap:
        scale = math.sqrt(cap / (width * height))
        width, height = int(width * scale), int(height * scale)
        image = image.resize((width, height), Image.Resampling.LANCZOS)
    grid_width, grid_height = width // SIDE_MULTIPLE * SIDE_MULTIPLE, height // SIDE_MULTIPLE * SIDE_MULTIPLE
    if grid_width == 0 or grid_height == 0:
        raise ValueError(
            f"Референс {width}x{height} слишком мал: при нескольких референсах каждая сторона "
            f"должна быть не меньше {SIDE_MULTIPLE} px"
        )
    left, top = (width - grid_width) // 2, (height - grid_height) // 2
    return image.crop((left, top, left + grid_width, top + grid_height))


def prepare_references(
    images: Sequence[Image.Image], width: int | None, height: int | None, *, max_pixels: int | None = None
) -> PreparedReferences:
    """Prepare the request's references by the reference rule (`generate.py`).

    Several references: each through `prepare_multi_reference`, no crop. One
    reference: `prepare_reference`; its crop box is kept only when the caller
    gave neither `width` nor `height`. No references: empty, no crop.
    `max_pixels` (`Flux3Options.reference_max_pixels`) lowers both pixel caps,
    so every prepared reference has at most that area (aspect kept, sides on
    the 16 grid, never upscaled); `None` is the reference behaviour.

    # Raises
    `ValueError` from `prepare_multi_reference` / `reference_canvas`.
    """
    if len(images) > 1:
        return PreparedReferences(
            images=tuple(prepare_multi_reference(image, max_pixels=max_pixels) for image in images), crop_box=None
        )
    if not images:
        return PreparedReferences(images=(), crop_box=None)
    prepared, box = prepare_reference(images[0], max_pixels=max_pixels)
    crop = box if width is None and height is None else None
    return PreparedReferences(images=(prepared,), crop_box=crop)


def default_size(references: Sequence[Image.Image], text_to_image: tuple[int, int]) -> tuple[int, int]:
    """The output `(width, height)` when the request names none: `text_to_image`
    (the sampler default) without references, else the first prepared
    reference's own size."""
    return references[0].size if references else text_to_image


def to_tensor(image: Image.Image) -> Tensor:
    """An RGB PIL image -> float32 `[3, H, W]` in [-1, 1] on the CPU.

    # Raises
    `ValueError` when the image is not RGB.
    """
    if image.mode != "RGB":
        raise ValueError(f"to_tensor: ожидается RGB-изображение, а не режим {image.mode!r}")
    return torch.from_numpy(np.asarray(image).copy()).permute(2, 0, 1).float() / 127.5 - 1.0


def to_pil(image: Tensor) -> Image.Image:
    """A `[3, H, W]` tensor in [-1, 1] -> RGB PIL image (clamped, truncated to
    uint8 as in the reference)."""
    array = ((image.float().clamp(-1, 1) + 1) * 127.5).to(torch.uint8).permute(1, 2, 0).cpu().numpy()
    return Image.fromarray(array)


def reference_tokens(encoded: Sequence[TokenPair], rope_scale: float) -> TokenPair | None:
    """Join the AE encodings of the references into the `image_cond` pair.

    `encoded[n]` is `AutoEncoder.encode` of reference `n`. Each reference's ids
    are scaled by `rope_scale` and get the l axis `-(1 + n)`, which tells the
    references apart; tokens become bf16. `None` without references.
    """
    if not encoded:
        return None
    tokens: list[Tensor] = []
    ids: list[Tensor] = []
    for index, (reference_tokens_n, reference_ids_n) in enumerate(encoded):
        scaled = scaled_ids(reference_ids_n, rope_scale)
        scaled[:, 3] = -(1 + index)
        tokens.append(reference_tokens_n.to(torch.bfloat16))
        ids.append(scaled)
    return torch.cat(tokens), torch.cat(ids)


def initial_noise(latent_height: int, latent_width: int, seed: int, channels: int) -> Tensor:
    """The reference's starting noise: `[h * w, channels]` standard normal from a
    CPU generator seeded with `seed`, rounded to bf16, returned as float32 on the
    CPU (bf16 values, fp32 state across the Euler steps).

    The CPU generator makes the noise identical on every device.
    """
    generator = torch.Generator(device="cpu").manual_seed(seed)
    noise = torch.randn((latent_height * latent_width, channels), generator=generator)
    return noise.to(torch.bfloat16).float()
