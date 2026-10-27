"""
File: modules/ai_backend/ipc/handlers/flux2_klein.py

Methods hosted here:
    inpaint.flux2_klein           — FLUX.2 klein region editing, streaming.
    inpaint.flux2_klein.status    — component availability + residency.
    inpaint.flux2_klein.unload    — drop the loaded pipeline.
    inpaint.flux2_klein.component_action — one per-component residency action, streaming.
    inpaint.flux2_klein.prompt_cache.build  — save the prompt as a named preset in the model's library (encodes when needed), streaming.
    inpaint.flux2_klein.prompt_cache.list   — the presets of the model's library.
    inpaint.flux2_klein.prompt_cache.export — copy an entry (by file) to any `.msprompt` path.
    inpaint.flux2_klein.prompt_cache.import — copy a `.msprompt` file into its library.

No method here downloads weights: every FLUX.2 klein download (4B and 9B) is
owned by the application's model catalog (Rust `ms_sysprobe::ai_models`).

The four `.prompt_cache.*` methods are registered here and run the bodies shared
with every model library (`prompt_cache_handlers.py`) over
`ctx.state.flux2_klein_inpaint`.

Streaming: like ``inpaint.flux_fill`` the handler pushes ``progress{id}`` frames
via the dispatcher's ``ProgressEmitter`` (``HandlerContext.progress_emitter``),
with a ``phase`` header field that is ``"load"`` (component being built) or
``"generate"`` (denoising step). No preview blob is ever sent.

Blob convention (the other inpaint methods' two segments, plus one optional):
    request blob = region_png ++ mask_png [++ reference_png]
                   (split via image_len / mask_len / reference_len)
`reference_len` is optional: absent or 0 means no reference. A reference is the
user's marks composited over a copy of the region, handed to the pipeline as its
`image_reference`; the service requires it to be exactly the region's size.
`mask_png` is the LATENT mask (L8, already dilated by the client), used as-is.
The result — the RAW decoded region as an RGB PNG, with no colour match and no
composite (the client post-processes it, protocol 17) — goes in the response
blob (raw bytes) and its length is repeated
as ``image_len`` in the response header, next to the OOM-recovery report and the
effective ``text_attention_in_mask`` (reported beside ``applied``, not in it,
because ``applied`` is persisted into the user's settings). ``applied`` is
``{vae_tiling}`` alone. The header also carries the large-model manager's view
of the run: the requested resource steps ``vram_step`` / ``ram_step``
(``params.vram_step`` / ``params.ram_step``, an absent axis ``auto``), the
effective ``vram_step_effective`` / ``ram_step_effective``, ``degraded`` and
``degrade_reason``. Retired fields — the placement wire (``placement``,
``profile``, the unload / slicing / low-memory flags) and the region
post-processing (``mask_dilate_px``, ``mask_feather_px``, ``color_match``,
``color_match_local_pct``, ``color_match_scale_px``) — are refused by name by
``params.normalize_flux2_klein_params``. Forecasts are ``large_models.profiles``
(``large_models.py``, the FLUX.2 adapter ``large_model_profiles``).
"""

from __future__ import annotations

import threading
import traceback
from typing import Any

from ..protocol import (
    METHOD_INPAINT_FLUX2_KLEIN,
    METHOD_INPAINT_FLUX2_KLEIN_COMPONENT_ACTION,
    METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_BUILD,
    METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_EXPORT,
    METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_IMPORT,
    METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_LIST,
    METHOD_INPAINT_FLUX2_KLEIN_STATUS,
    METHOD_INPAINT_FLUX2_KLEIN_UNLOAD,
)
from ..registry import HandlerContext, Interrupted, register
from .prompt_cache_handlers import handle_build, handle_export, handle_import, handle_list, require_non_empty_str
from .streaming import progress_forwarder, require_params, require_segment_len

#: The settings reported back as `applied` (`PROTOCOL.md §5.4`): the ones the
#: client persists after a run. Every name here is present in every response.
_APPLIED_FLAGS = ("vae_tiling",)

#: The run's step keys copied from the service answer (requested / effective
#: steps). The handler imports no service code at module level, so this mirrors
#: `inpaint.flux2_klein.memory.STEP_FIELD_KEYS`; `test_flux2_klein` guards the mirror.
_STEP_KEYS = ("vram_step", "ram_step", "vram_step_effective", "ram_step_effective")


def _split_request_blob(
    header: dict[str, Any], blob: bytes
) -> tuple[bytes, bytes, bytes | None]:
    """Split the request blob into region PNG + mask PNG + optional reference PNG.

    Strict equality is required: `image_len + mask_len + reference_len` must be
    exactly the blob length, so a truncated or over-long frame is a request error
    rather than a silently mis-sliced image. `reference_len` absent or `0` means
    the request carries no reference, and the third value is `None`.
    """
    image_len = require_segment_len(header, "image_len", optional=False)
    mask_len = require_segment_len(header, "mask_len", optional=False)
    reference_len = require_segment_len(header, "reference_len", optional=True)
    if image_len + mask_len + reference_len != len(blob):
        raise ValueError(
            f"Inpaint blob length mismatch: image_len ({image_len}) + mask_len "
            f"({mask_len}) + reference_len ({reference_len}) != blob length ({len(blob)})."
        )
    image_png = blob[:image_len]
    mask_png = blob[image_len : image_len + mask_len]
    reference_png = blob[image_len + mask_len :] if reference_len else None
    if not image_png:
        raise ValueError("inpaint.flux2_klein requires a non-empty region image in the blob.")
    if not mask_png:
        raise ValueError("inpaint.flux2_klein requires a non-empty mask in the blob.")
    return image_png, mask_png, reference_png


def _handle_inpaint_flux2_klein(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    if cancel_event.is_set():
        raise Interrupted("inpaint.flux2_klein canceled before start.")

    image_png, mask_png, reference_png = _split_request_blob(header, blob)
    params_raw = require_params(header)
    on_progress = progress_forwarder(ctx, METHOD_INPAINT_FLUX2_KLEIN)

    try:
        result = ctx.state.flux2_klein_inpaint.inpaint_image_bytes(
            image_png,
            mask_png,
            reference_bytes=reference_png,
            params=params_raw,
            progress_callback=on_progress,
        )
    except (ValueError, FileNotFoundError):
        raise
    except Exception:  # noqa: BLE001
        if cancel_event.is_set():
            raise Interrupted("inpaint.flux2_klein canceled.") from None
        traceback.print_exc()
        raise

    if cancel_event.is_set():
        raise Interrupted("inpaint.flux2_klein canceled.")

    image_png_out = result.get("image_png", b"") or b""
    applied = result.get("applied", {})
    fields = {
        "image_len": len(image_png_out),
        "oom_recovered": bool(result.get("oom_recovered", False)),
        # Every flag, always: the client persists `applied` after a run
        # (`Flux2AppliedFlags` in
        # `crates/ms-tab-cleaning/src/tools/ai_editor/engines/flux2_klein/progress.rs`).
        "applied": {name: bool(applied.get(name, False)) for name in _APPLIED_FLAGS},
        # The EFFECTIVE value (false under `whole_region`), reported beside
        # `applied` rather than inside it: `applied` is persisted into the user's
        # settings, and a mode-forced off must not overwrite the user's choice.
        "text_attention_in_mask": bool(result.get("text_attention_in_mask", False)),
        # The large-model manager's view of the run: the steps asked for, the
        # ones that actually ran, and whether the degrade ladder changed them.
        **{key: result.get(key) for key in _STEP_KEYS},
        "degraded": bool(result.get("degraded", False)),
        "degrade_reason": result.get("degrade_reason"),
    }
    return fields, image_png_out


register(METHOD_INPAINT_FLUX2_KLEIN, _handle_inpaint_flux2_klein)


def _handle_inpaint_flux2_klein_status(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    # `params` is optional here: the UI asks for status while the user is still
    # picking component paths.
    params_raw = require_params(header)
    return dict(ctx.state.flux2_klein_inpaint.status(params_raw or None)), b""


register(METHOD_INPAINT_FLUX2_KLEIN_STATUS, _handle_inpaint_flux2_klein_status)


def _handle_inpaint_flux2_klein_unload(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    unloaded = bool(ctx.state.flux2_klein_inpaint.unload())
    return {"unloaded": unloaded}, b""


register(METHOD_INPAINT_FLUX2_KLEIN_UNLOAD, _handle_inpaint_flux2_klein_unload)


def _handle_inpaint_flux2_klein_component_action(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """Perform one per-component residency action; no image is produced.

    Streams the same `phase:"load"` frames as a generation, because a component
    load reads up to 18 GB from disk and a silent wait is not acceptable.

    `component` and `action` are only checked to have ARRIVED here; which values
    are legal, and which are possible for that component right now, is the
    service's single answer (`_require_component_name` / `_require_action_name` /
    `_require_action_available_locked`) — duplicating the matrix in this layer is
    exactly the drift the contract puts it in one place to avoid.

    Cancellation follows the shared inpaint contract: checked before the call and
    after it returns, never inside a weight read.
    """
    if cancel_event.is_set():
        raise Interrupted("inpaint.flux2_klein.component_action canceled before start.")
    params_raw = require_params(header)
    component = require_non_empty_str(header, "component")
    action = require_non_empty_str(header, "action")
    try:
        result = ctx.state.flux2_klein_inpaint.component_action(
            params_raw,
            component=component,
            action=action,
            progress_callback=progress_forwarder(ctx, METHOD_INPAINT_FLUX2_KLEIN_COMPONENT_ACTION),
        )
    except (ValueError, FileNotFoundError):
        raise
    except Exception:  # noqa: BLE001
        if cancel_event.is_set():
            raise Interrupted("inpaint.flux2_klein.component_action canceled.") from None
        traceback.print_exc()
        raise
    if cancel_event.is_set():
        raise Interrupted("inpaint.flux2_klein.component_action canceled.")
    return dict(result), b""


register(METHOD_INPAINT_FLUX2_KLEIN_COMPONENT_ACTION, _handle_inpaint_flux2_klein_component_action)


# ---------------------------------------------------------------------------
# Prompt-cache library (bodies shared with every model library:
# `prompt_cache_handlers`; the cancel and error texts name the method)
# ---------------------------------------------------------------------------
def _handle_prompt_cache_build(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """Save the prompt as a named preset of the model's library (`prompt_cache_handlers.handle_build`).

    Only the prompt phase's steps stream, because no pipeline is built.
    """
    return handle_build(ctx, header, cancel_event, service=ctx.state.flux2_klein_inpaint, method=METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_BUILD)


register(METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_BUILD, _handle_prompt_cache_build)


def _handle_prompt_cache_list(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """List the presets of the library named in `params.prompt_library` (`prompt_cache_handlers.handle_list`)."""
    return handle_list(ctx, header, cancel_event, service=ctx.state.flux2_klein_inpaint, method=METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_LIST)


register(METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_LIST, _handle_prompt_cache_list)


def _handle_prompt_cache_export(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """Copy the library entry `file` to the `.msprompt` path in `path` (`prompt_cache_handlers.handle_export`)."""
    return handle_export(ctx, header, cancel_event, service=ctx.state.flux2_klein_inpaint, method=METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_EXPORT)


register(METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_EXPORT, _handle_prompt_cache_export)


def _handle_prompt_cache_import(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """Copy a `.msprompt` file into the library it belongs to (`prompt_cache_handlers.handle_import`)."""
    return handle_import(ctx, header, cancel_event, service=ctx.state.flux2_klein_inpaint, method=METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_IMPORT)


register(METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_IMPORT, _handle_prompt_cache_import)
