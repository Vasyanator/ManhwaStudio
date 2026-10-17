"""
File: modules/ai_backend/inpaint/flux3_klein/region.py

Purpose:
FLUX.3 klein as the cleaning tab's region editor (IPC `inpaint.flux3_klein`):
a region PNG, a mask PNG and up to `wire.MAX_EXTRA_REFERENCES` reference PNGs
in, the edited region PNG out. FLUX.3 has no mask input, so the region is
reference #0 and the output is the region's size; the painted mask is a
PERMISSION TO CHANGE: the generated pixels are composited back inside it
(feathered inwards, color-matched to the ring outside it — globally, then
locally in CIELAB unless `color_match_local_pct` is 0), so every pixel outside
the mask comes back byte-identical.

Key functions:
- `edit_region_bytes(service, image_png, mask_png, reference_pngs, params, *,
  progress_callback, should_cancel)` -> the answer dict (module notes);
- `progress_stages()` — the stable `stage` ids of the progress frames, in run order.

Progress frames (`progress_callback(phase, step, total, label, *, stage=)`):
`phase:"load"` for every step but the denoise — `stage` = `"prepare"` (step 0)
or the observer `Phase` value, `step` = its position in `progress_stages()`,
`total` = the last position; `phase:"generate"` for the denoise, `step` /
`total` = flow-map steps, `stage` = `"denoise"`. `label` is Russian log text;
the client localizes `stage`.

Cancellation: `should_cancel()` is checked at every observer phase begin and
after every denoise step; a raised `service.Flux3Canceled` ends the run (the
service keeps whatever the manager's `finish` leaves, no unload).

Notes:
- The seed: `params.seed` `null` draws `secrets.randbits(32)`; the answer
  echoes the seed that ran, so the client can pin it.
- Refusals before any weight is read: params (`wire.normalize_flux3_params`),
  the region size (`wire.validate_region_size`), the mask (L8, the region's
  size, non-empty; solid under `whole_region`), the references (count,
  decodable).
- Torch is reached only through the service; this module needs numpy and PIL.
"""

from __future__ import annotations

import logging
import secrets
from collections.abc import Callable, Mapping, Sequence
from typing import Any

from ..region_imaging import composite_over_region, decode_image_rgb, decode_mask, encode_png_rgb, match_color_local, match_color_outside_mask, require_solid_mask
from .params import Flux3Request
from .phases import Phase
from .service import Flux3Canceled, Flux3KleinService
from .wire import MAX_EXTRA_REFERENCES, normalize_flux3_params, validate_region_size

log = logging.getLogger(__name__)

#: `progress_callback(phase, step, total, label, *, stage=None)`.
ProgressCallback = Callable[..., None]

#: The first progress stage, before the service is called.
STAGE_PREPARE = "prepare"

#: Log labels of the progress stages (the client shows its own text per `stage`).
_LABELS: Mapping[str, str] = {
    STAGE_PREPARE: "Подготовка FLUX.3 klein",
    Phase.DIT_LOAD.value: "Загрузка DiT",
    Phase.AE_LOAD.value: "Загрузка автоэнкодера",
    Phase.DIT_OFFLOAD.value: "Размещение DiT",
    Phase.ENCODER_LOAD.value: "Загрузка текстового энкодера",
    Phase.ENCODE.value: "Кодирование промпта",
    Phase.ENCODER_RELEASE.value: "Освобождение энкодера",
    Phase.REF_ENCODE.value: "Кодирование референсов",
    Phase.DIT_RESTORE.value: "Возврат DiT",
    Phase.DENOISE.value: "Генерация",
    Phase.DIT_OFFLOAD_DECODE.value: "Размещение для декодирования",
    Phase.DECODE.value: "Декодирование",
}


def progress_stages() -> tuple[str, ...]:
    """Every `stage` id of the progress frames, in run order: `prepare`, then the observer `Phase` values."""
    return (STAGE_PREPARE, *(phase.value for phase in Phase))


def _decode_references(reference_pngs: Sequence[bytes]) -> list[Any]:
    """The user references as PIL RGB images (#1 and up).

    # Raises
    `ValueError` for more than `MAX_EXTRA_REFERENCES` or an empty / undecodable one.
    """
    from PIL import Image, UnidentifiedImageError

    if len(reference_pngs) > MAX_EXTRA_REFERENCES:
        raise ValueError(f"FLUX.3 klein: слишком много референсов: {len(reference_pngs)}, максимум {MAX_EXTRA_REFERENCES}")
    images = []
    for index, data in enumerate(reference_pngs, start=1):
        if not data:
            raise ValueError(f"FLUX.3 klein: референс №{index} пуст")
        try:
            images.append(Image.fromarray(decode_image_rgb(data), "RGB"))
        except (UnidentifiedImageError, OSError, ValueError) as exc:
            raise ValueError(f"FLUX.3 klein: референс №{index} не читается как изображение: {exc}") from exc
    return images


def edit_region_bytes(
    service: Flux3KleinService,
    image_png: bytes,
    mask_png: bytes,
    reference_pngs: Sequence[bytes],
    params: Mapping[str, Any] | None,
    *,
    progress_callback: ProgressCallback | None = None,
    should_cancel: Callable[[], bool] | None = None,
) -> dict[str, Any]:
    """Edit one region with FLUX.3 klein; returns the `inpaint.flux3_klein` answer.

    The answer: `image_png` (the composite, the region's size), `seed`,
    `steps`, `width`, `height`, `text_tokens`, `applied` (`format`,
    `fp8_compute` or `None`, `compute_path`, `encoder_mode`, `ae_dtype`), the
    step keys `vram_step`, `ram_step`, `vram_step_effective`,
    `ram_step_effective`, `degraded`, `degrade_reason`.

    # Raises
    `ValueError` for the refusals in the module notes and for a generated
    image that is not exactly the region's size; `Flux3Canceled` when
    `should_cancel()` turned true; whatever `service.generate` raises
    (`FileNotFoundError`, `LargeModelRefused`, `LargeModelRunFailed`, ...).
    """
    import numpy as np
    from PIL import Image

    wire = normalize_flux3_params(params, require_prompt=True)
    region_rgb = decode_image_rgb(image_png)
    height, width = region_rgb.shape[:2]
    validate_region_size(width, height)
    mask_u8 = decode_mask(mask_png, expected_hw=(height, width))
    if wire.whole_region:
        require_solid_mask(mask_u8)
    elif not mask_u8.any():
        raise ValueError("FLUX.3 klein: маска пуста — нечего менять. Закрасьте область изменения или включите режим «без маски».")
    references = _decode_references(reference_pngs)
    seed = wire.seed if wire.seed is not None else secrets.randbits(32)
    stages = progress_stages()
    last = len(stages) - 1

    def report(phase: str, step: int, total: int, stage: str) -> None:
        if progress_callback is not None:
            progress_callback(phase, step, total, _LABELS[stage], stage=stage)

    def check_cancel() -> None:
        if should_cancel is not None and should_cancel():
            raise Flux3Canceled("FLUX.3 klein: генерация отменена")

    check_cancel()
    report("load", 0, last, STAGE_PREPARE)

    def observer(phase: Phase, event: str, info: Mapping[str, Any]) -> None:
        if event != "begin":
            return
        check_cancel()
        if phase is Phase.DENOISE:
            report("generate", 0, int(info["steps"]), phase.value)
        else:
            report("load", stages.index(phase.value), last, phase.value)

    def on_step(step: int, total: int) -> None:
        report("generate", step, total, Phase.DENOISE.value)
        check_cancel()

    request = Flux3Request(caption=wire.prompt, references=(Image.fromarray(region_rgb, "RGB"), *references), width=width, height=height, seed=seed, steps=wire.steps)
    log.info("FLUX.3 klein: правка области %dx%d, %d доп. референсов, сид %d, %s", width, height, len(references), seed, "вся область" if wire.whole_region else "по маске")
    result = service.generate(request, wire.options, observer=observer, step_callback=on_step)
    generated = np.ascontiguousarray(np.asarray(result.image.convert("RGB"), dtype=np.uint8))
    if generated.shape[:2] != (height, width):
        raise ValueError(f"FLUX.3 klein вернул изображение {generated.shape[1]}x{generated.shape[0]} вместо области {width}x{height}; композит невозможен")
    if wire.color_match:
        generated = match_color_outside_mask(generated, region_rgb, mask_u8)
        if wire.color_match_local_pct > 0:
            generated = match_color_local(region_rgb, generated, mask_u8, strength=wire.color_match_local_pct / 100.0, scale_px=wire.color_match_scale_px)
    composed = composite_over_region(region_rgb, generated, mask_u8, wire.mask_feather_px)
    applied = result.applied
    return {
        "image_png": encode_png_rgb(composed),
        "seed": int(result.seed),
        "steps": int(result.steps),
        "width": int(width),
        "height": int(height),
        "text_tokens": int(result.text_tokens),
        "applied": {
            "format": applied.variant.value,
            "fp8_compute": applied.fp8_compute.value if applied.fp8_compute is not None else None,
            "compute_path": applied.compute_path,
            "encoder_mode": applied.encoder_mode.value,
            "ae_dtype": applied.ae_dtype.value,
        },
        "vram_step": applied.steps_requested.vram.value,
        "ram_step": applied.steps_requested.ram.value,
        "vram_step_effective": applied.steps_effective.vram.value,
        "ram_step_effective": applied.steps_effective.ram.value,
        "degraded": bool(applied.degraded),
        "degrade_reason": applied.degrade_reason,
    }
