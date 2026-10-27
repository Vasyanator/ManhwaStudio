"""
File: modules/ai_backend/ipc/handlers/flux3_klein.py

Methods hosted here (protocol v15, `PROTOCOL.md` "FLUX.3 klein"):
    inpaint.flux3_klein        — FLUX.3 klein region editing, streaming, mid-run cancel.
    inpaint.flux3_klein.status — package files per format, the prompt-cache keys + the service health.
    inpaint.flux3_klein.unload — drop every idle FLUX.3 handle.
    inpaint.flux3_klein.prompt_cache.build  — save the caption as a named preset of the FLUX.3 library (TEXT mode; encodes when needed), streaming.
    inpaint.flux3_klein.prompt_cache.list   — the presets of the FLUX.3 library.
    inpaint.flux3_klein.prompt_cache.export — copy an entry (by file) to any `.msprompt` path.
    inpaint.flux3_klein.prompt_cache.import — copy a v2 `.msprompt` file of the FLUX.3 library into it.
The package is downloaded by the application's model catalog (Rust); the
former `inpaint.flux3_klein.download.*` methods are gone since v24 and the
dispatcher answers them as unknown methods.

Blob convention of `inpaint.flux3_klein`:
    request blob = region_png ++ mask_png ++ reference_png_0 ++ ... ++ reference_png_{n-1}
                   (split via image_len / mask_len / reference_lens, n <= 9)
The lengths must sum to the blob length exactly; a reference length of 0 is a
request error. The mask travels for validation only. The response blob is the
RAW generated region as an RGB PNG, with no colour match and no composite (the
client post-processes it, protocol 17; its length repeated as `image_len`); the header carries `seed`, `steps`, `width`, `height`,
`text_tokens`, `applied` and the step keys (`region.edit_region_bytes`).

Streaming: `progress{id}` frames through the request's `ProgressEmitter`
(`streaming.progress_forwarder`) with `phase` `load` / `generate`,
`step`, `total`, `label` and a stable `stage` id; a run's
checkpoint reads add the byte level `file_step` / `file_total` / `file_label` /
`file_component` (`ipc/PROTOCOL.md`, FLUX.3 streaming detail).

Cancellation is observed INSIDE the run (every phase begin and every denoise
step: `Flux3Canceled` -> `Interrupted`).

The four `.prompt_cache.*` methods run the bodies shared with every model
library (`prompt_cache_handlers.py`); the build goes through
`region.build_prompt_cache` (`_RegionPromptBuild`), so its frames are the run's
(`stage` `prepare` / `encoder_load` / `encode` / `encoder_release`) and its
cancel is checked at every phase begin as well as around the call.

The service is `ctx.state.flux3_klein` (`Flux3KleinService`); the run's
region semantics live in `inpaint/flux3_klein/region.py`. Torch-free at import.
"""

from __future__ import annotations

import threading
import traceback
from typing import Any

from ..protocol import (
    METHOD_INPAINT_FLUX3_KLEIN,
    METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_BUILD,
    METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_EXPORT,
    METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_IMPORT,
    METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_LIST,
    METHOD_INPAINT_FLUX3_KLEIN_STATUS,
    METHOD_INPAINT_FLUX3_KLEIN_UNLOAD,
)
from ..registry import HandlerContext, Interrupted, register
from .prompt_cache_handlers import handle_build, handle_export, handle_import, handle_list
from .streaming import progress_forwarder, require_params, require_segment_len

#: Most user references a request may carry (`inpaint.flux3_klein.wire.MAX_EXTRA_REFERENCES`,
#: mirrored here so this layer stays import-free; `test_flux3_klein` guards the mirror).
MAX_REFERENCES = 9


def _split_request_blob(header: dict[str, Any], blob: bytes) -> tuple[bytes, bytes, list[bytes]]:
    """Split the request blob into region PNG + mask PNG + the reference PNGs (strict lengths).

    # Raises
    `ValueError` for a missing / negative segment length, a `reference_lens`
    that is not an array of positive integers of at most `MAX_REFERENCES`
    entries, an empty region or mask, or lengths that do not sum to the blob.
    """
    image_len = require_segment_len(header, "image_len", optional=False)
    mask_len = require_segment_len(header, "mask_len", optional=False)
    raw = header.get("reference_lens")
    if raw is None:
        raw = []
    if not isinstance(raw, list) or len(raw) > MAX_REFERENCES:
        raise ValueError(f"Field 'reference_lens' must be an array of at most {MAX_REFERENCES} integers.")
    reference_lens: list[int] = []
    for index, value in enumerate(raw):
        if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
            raise ValueError(f"Field 'reference_lens[{index}]' must be a positive integer.")
        reference_lens.append(value)
    total = image_len + mask_len + sum(reference_lens)
    if total != len(blob):
        raise ValueError(f"Inpaint blob length mismatch: image_len ({image_len}) + mask_len ({mask_len}) + reference_lens ({sum(reference_lens)}) != blob length ({len(blob)}).")
    if not image_len:
        raise ValueError("inpaint.flux3_klein requires a non-empty region image in the blob.")
    if not mask_len:
        raise ValueError("inpaint.flux3_klein requires a non-empty mask in the blob.")
    image_png = blob[:image_len]
    mask_png = blob[image_len : image_len + mask_len]
    references: list[bytes] = []
    offset = image_len + mask_len
    for length in reference_lens:
        references.append(blob[offset : offset + length])
        offset += length
    return image_png, mask_png, references


def _handle_inpaint_flux3_klein(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """Edit one region; readable refusals reach the user verbatim, a cancel is `Interrupted`."""
    from ...inpaint.flux3_klein import region
    from ...inpaint.flux3_klein.service import Flux3Canceled

    if cancel_event.is_set():
        raise Interrupted("inpaint.flux3_klein canceled before start.")
    image_png, mask_png, references = _split_request_blob(header, blob)
    params = require_params(header)
    try:
        result = region.edit_region_bytes(
            ctx.state.flux3_klein,
            image_png,
            mask_png,
            references,
            params,
            progress_callback=progress_forwarder(ctx, METHOD_INPAINT_FLUX3_KLEIN),
            should_cancel=cancel_event.is_set,
        )
    except Flux3Canceled:
        raise Interrupted("inpaint.flux3_klein canceled.") from None
    except (ValueError, FileNotFoundError):
        raise
    except Exception:  # noqa: BLE001 - logged with the traceback, then re-raised
        if cancel_event.is_set():
            raise Interrupted("inpaint.flux3_klein canceled.") from None
        traceback.print_exc()
        raise
    image = result.pop("image_png")
    return {"image_len": len(image), **result}, image


register(METHOD_INPAINT_FLUX3_KLEIN, _handle_inpaint_flux3_klein)


def _handle_status(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`Flux3KleinService.status(params)`: file checks only, never a load."""
    return dict(ctx.state.flux3_klein.status(require_params(header))), b""


register(METHOD_INPAINT_FLUX3_KLEIN_STATUS, _handle_status)


def _handle_unload(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`{unloaded}`: whether any idle FLUX.3 handle held weights."""
    return {"unloaded": bool(ctx.state.flux3_klein.unload())}, b""


register(METHOD_INPAINT_FLUX3_KLEIN_UNLOAD, _handle_unload)


# ---------------------------------------------------------------------------
# Prompt-cache library (bodies shared with every model library:
# `prompt_cache_handlers`; the cancel and error texts name the method)
# ---------------------------------------------------------------------------
class _RegionPromptBuild:
    """The `prompt_cache_build(params, *, name, progress_callback)` shape `handle_build` calls, over `region.build_prompt_cache`.

    It adds the run's frame builder (stages, disk-read frames) and the
    in-call cancel check (`Flux3Canceled` at a phase begin, which
    `handle_build` answers as `Interrupted` because `cancel_event` is set).
    """

    def __init__(self, service: Any, cancel_event: threading.Event) -> None:
        self._service = service
        self._cancel_event = cancel_event

    def prompt_cache_build(self, params: dict[str, Any], *, name: str | None, progress_callback: Any) -> dict[str, Any]:
        from ...inpaint.flux3_klein import region

        return region.build_prompt_cache(self._service, params, name=name, progress_callback=progress_callback, should_cancel=self._cancel_event.is_set)


def _handle_prompt_cache_build(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """Save the caption as a named preset of the FLUX.3 library (`prompt_cache_handlers.handle_build`); only the encoder stages stream."""
    service = _RegionPromptBuild(ctx.state.flux3_klein, cancel_event)
    return handle_build(ctx, header, cancel_event, service=service, method=METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_BUILD)


register(METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_BUILD, _handle_prompt_cache_build)


def _handle_prompt_cache_list(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """List the presets of the library named in `params.prompt_library` (`prompt_cache_handlers.handle_list`)."""
    return handle_list(ctx, header, cancel_event, service=ctx.state.flux3_klein, method=METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_LIST)


register(METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_LIST, _handle_prompt_cache_list)


def _handle_prompt_cache_export(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """Copy the library entry `file` to the `.msprompt` path in `path` (`prompt_cache_handlers.handle_export`)."""
    return handle_export(ctx, header, cancel_event, service=ctx.state.flux3_klein, method=METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_EXPORT)


register(METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_EXPORT, _handle_prompt_cache_export)


def _handle_prompt_cache_import(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """Copy a v2 `.msprompt` file of the FLUX.3 library into it (`prompt_cache_handlers.handle_import`)."""
    return handle_import(ctx, header, cancel_event, service=ctx.state.flux3_klein, method=METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_IMPORT)


register(METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_IMPORT, _handle_prompt_cache_import)
