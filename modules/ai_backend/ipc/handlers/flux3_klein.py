"""
File: modules/ai_backend/ipc/handlers/flux3_klein.py

Methods hosted here (protocol v15, `PROTOCOL.md` "FLUX.3 klein"):
    inpaint.flux3_klein                — FLUX.3 klein region editing, streaming, mid-run cancel.
    inpaint.flux3_klein.status         — package files per format + the service health.
    inpaint.flux3_klein.unload         — drop every idle FLUX.3 handle.
    inpaint.flux3_klein.download.check — repository access state + plan totals of one format.
    inpaint.flux3_klein.download.start — fetch one format's missing files, streaming.

Blob convention of `inpaint.flux3_klein`:
    request blob = region_png ++ mask_png ++ reference_png_0 ++ ... ++ reference_png_{n-1}
                   (split via image_len / mask_len / reference_lens, n <= 9)
The lengths must sum to the blob length exactly; a reference length of 0 is a
request error. The response blob is the edited region PNG (its length repeated
as `image_len`); the header carries `seed`, `steps`, `width`, `height`,
`text_tokens`, `applied` and the step keys (`region.edit_region_bytes`).

Streaming: `progress{id}` frames through the request's `ProgressEmitter`
(`streaming.progress_forwarder`) with `phase` `load` / `generate` / `download`,
`step`, `total`, `label` and, for the run, a stable `stage` id.

Cancellation is observed INSIDE the work for the run (every phase begin and
every denoise step: `Flux3Canceled` -> `Interrupted`) and for `download.start`
(every chunk and retry wait: `DownloadCanceled` -> `Interrupted`).

The service is `ctx.state.flux3_klein` (`Flux3KleinService`); the run's
region semantics live in `inpaint/flux3_klein/region.py`, the download in
`inpaint/flux3_download.py` (stateless, imported lazily). Torch-free at import.
"""

from __future__ import annotations

import threading
import traceback
from typing import Any

from ..protocol import (
    METHOD_INPAINT_FLUX3_KLEIN,
    METHOD_INPAINT_FLUX3_KLEIN_DOWNLOAD_CHECK,
    METHOD_INPAINT_FLUX3_KLEIN_DOWNLOAD_START,
    METHOD_INPAINT_FLUX3_KLEIN_STATUS,
    METHOD_INPAINT_FLUX3_KLEIN_UNLOAD,
)
from ..registry import HandlerContext, Interrupted, register
from .streaming import progress_forwarder, read_hf_token, require_params, require_segment_len

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


def _download_module() -> Any:
    """The FLUX.3 download module, imported lazily (stateless: no `AppState` field, no `huggingface_hub` at import)."""
    from ...inpaint import flux3_download

    return flux3_download


def _handle_download_check(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """Repository access and the plan of the requested `format`, echoed back; no bytes fetched."""
    if cancel_event.is_set():
        raise Interrupted("inpaint.flux3_klein.download.check canceled before start.")
    return dict(_download_module().check_access(read_hf_token(header), weight_format=header.get("format"))), b""


register(METHOD_INPAINT_FLUX3_KLEIN_DOWNLOAD_CHECK, _handle_download_check)


def _handle_download_start(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """Download one format's missing files, streaming two-level byte progress; cancel stops the socket."""
    if cancel_event.is_set():
        raise Interrupted("inpaint.flux3_klein.download.start canceled before start.")
    downloads = _download_module()
    try:
        result = downloads.download(
            read_hf_token(header),
            weight_format=header.get("format"),
            progress_callback=progress_forwarder(ctx, METHOD_INPAINT_FLUX3_KLEIN_DOWNLOAD_START),
            should_cancel=cancel_event.is_set,
        )
    except downloads.DownloadCanceled:
        raise Interrupted("inpaint.flux3_klein.download.start canceled.") from None
    except (ValueError, FileNotFoundError, RuntimeError):
        # Readable refusals (no token, no access, no space, changed repo layout).
        raise
    except Exception:  # noqa: BLE001 - logged with the traceback, then re-raised
        if cancel_event.is_set():
            raise Interrupted("inpaint.flux3_klein.download.start canceled.") from None
        traceback.print_exc()
        raise
    return dict(result), b""


register(METHOD_INPAINT_FLUX3_KLEIN_DOWNLOAD_START, _handle_download_start)
