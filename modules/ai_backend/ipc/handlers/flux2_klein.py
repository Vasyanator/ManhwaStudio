"""
File: modules/ai_backend/ipc/handlers/flux2_klein.py

Methods hosted here:
    inpaint.flux2_klein           — FLUX.2 klein region editing, streaming.
    inpaint.flux2_klein.status    — component availability + residency.
    inpaint.flux2_klein.unload    — drop the loaded pipeline.
    inpaint.flux2_klein.component_action — one per-component residency action, streaming.
    inpaint.flux2_klein.prompt_cache.build  — encode the prompt only, streaming.
    inpaint.flux2_klein.prompt_cache.list   — library entries of this encoder.
    inpaint.flux2_klein.prompt_cache.save   — store the cached prompt by name.
    inpaint.flux2_klein.prompt_cache.load   — put a stored entry back in the cache.
    inpaint.flux2_klein.prompt_cache.export — copy an entry to any `.msprompt` path.
    inpaint.flux2_klein.prompt_cache.import — copy a `.msprompt` file into the library.
    inpaint.flux2_klein.download.check      — per-repo access state + plan totals.
    inpaint.flux2_klein.download.start      — fetch the missing weights, streaming.

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
    METHOD_INPAINT_FLUX2_KLEIN_DOWNLOAD_CHECK,
    METHOD_INPAINT_FLUX2_KLEIN_DOWNLOAD_START,
    METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_BUILD,
    METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_EXPORT,
    METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_IMPORT,
    METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_LIST,
    METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_LOAD,
    METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_SAVE,
    METHOD_INPAINT_FLUX2_KLEIN_STATUS,
    METHOD_INPAINT_FLUX2_KLEIN_UNLOAD,
)
from ..registry import HandlerContext, Interrupted, register
from .streaming import progress_forwarder, read_hf_token, require_params, require_segment_len

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


def _require_non_empty_str(header: dict[str, Any], field: str) -> str:
    """Read a required non-empty string header field.

    The value itself is NOT validated here: a path or a cache name is untrusted
    input and the service is the single owner of what makes one acceptable
    (`require_prompt_file_source`, `sanitize_name_component`). This layer only
    guarantees the field arrived and is a string.
    """
    value = header.get(field)
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"Field '{field}' must be a non-empty string.")
    return value


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
    component = _require_non_empty_str(header, "component")
    action = _require_non_empty_str(header, "action")
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
# Prompt-cache library
# ---------------------------------------------------------------------------
def _handle_prompt_cache_build(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """Encode the prompt into the in-memory cache; no image is produced.

    Streams the same `phase:"load"` frames as a generation — reading the text
    encoder takes tens of seconds and a silent wait is not acceptable — but only the
    prompt phase's steps occur, because no pipeline is built.

    Cancellation follows the shared inpaint contract: checked before the call and
    after it returns, never inside the encoder read.
    """
    if cancel_event.is_set():
        raise Interrupted("inpaint.flux2_klein.prompt_cache.build canceled before start.")
    params_raw = require_params(header)
    try:
        result = ctx.state.flux2_klein_inpaint.prompt_cache_build(
            params_raw, progress_callback=progress_forwarder(ctx, METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_BUILD)
        )
    except (ValueError, FileNotFoundError):
        raise
    except Exception:  # noqa: BLE001
        if cancel_event.is_set():
            raise Interrupted("inpaint.flux2_klein.prompt_cache.build canceled.") from None
        traceback.print_exc()
        raise
    if cancel_event.is_set():
        raise Interrupted("inpaint.flux2_klein.prompt_cache.build canceled.")
    return dict(result), b""


register(METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_BUILD, _handle_prompt_cache_build)


def _handle_prompt_cache_list(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """List the library entries of the encoder named in `params`."""
    params_raw = require_params(header)
    return dict(ctx.state.flux2_klein_inpaint.prompt_cache_list(params_raw)), b""


register(METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_LIST, _handle_prompt_cache_list)


def _handle_prompt_cache_save(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """Store the cached prompt under `name`; `overwrite` must be explicit."""
    params_raw = require_params(header)
    name = _require_non_empty_str(header, "name")
    overwrite = bool(header.get("overwrite", False))
    result = ctx.state.flux2_klein_inpaint.prompt_cache_save(
        params_raw, name, overwrite=overwrite
    )
    return dict(result), b""


register(METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_SAVE, _handle_prompt_cache_save)


def _handle_prompt_cache_load(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """Load a library entry of the current family back into the cache."""
    params_raw = require_params(header)
    name = _require_non_empty_str(header, "name")
    return dict(ctx.state.flux2_klein_inpaint.prompt_cache_load(params_raw, name)), b""


register(METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_LOAD, _handle_prompt_cache_load)


def _handle_prompt_cache_export(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """Copy a library entry to the `.msprompt` path in `path`."""
    params_raw = require_params(header)
    name = _require_non_empty_str(header, "name")
    path = _require_non_empty_str(header, "path")
    result = ctx.state.flux2_klein_inpaint.prompt_cache_export(params_raw, name, path)
    return dict(result), b""


register(METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_EXPORT, _handle_prompt_cache_export)


def _handle_prompt_cache_import(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """Copy a `.msprompt` file into the library.

    `name` is optional (the file's own stem is used); `family_matches` in the
    answer tells the client whether the imported entry belongs to the encoder
    that is selected right now — the import itself never fails over that, because
    the file is filed under ITS OWN family and would otherwise be lost.
    """
    params_raw = require_params(header)
    path = _require_non_empty_str(header, "path")
    name_raw = header.get("name")
    if name_raw is not None and not isinstance(name_raw, str):
        raise ValueError("Field 'name' must be a string when present.")
    name = name_raw.strip() if isinstance(name_raw, str) and name_raw.strip() else None
    overwrite = bool(header.get("overwrite", False))
    result = ctx.state.flux2_klein_inpaint.prompt_cache_import(
        params_raw, path, name=name, overwrite=overwrite
    )
    return dict(result), b""


register(METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_IMPORT, _handle_prompt_cache_import)


# ---------------------------------------------------------------------------
# Model download from Hugging Face
# ---------------------------------------------------------------------------
def _read_variant(header: dict[str, Any]) -> Any:
    """Read the `variant` request field verbatim; absent or `null` means `None`.

    The value is NOT validated here. Which variants exist, what an absent field
    defaults to (`"9b"`) and what an unknown name costs are the downloader's
    single answer (`flux2_download.resolve_variant`), exactly as `component` and
    `action` are the service's — duplicating the vocabulary in this layer is the
    drift the contract puts it in one place to avoid.
    """
    return header.get("variant")


def _download_module() -> "Any":
    """The FLUX.2 download module, imported lazily.

    It is a stateless function module — no resident weights, no lease, no
    residency — so it is NOT an `AppState` service and gets no field there;
    importing it here on demand is the same shape `handlers/sdxl.py` uses for the
    inpaint helper it shares, and it keeps this handler module free of
    `huggingface_hub` at import time.
    """
    from ...inpaint import flux2_download

    return flux2_download


def _handle_download_check(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """Report per-repository access and the plan totals; no bytes are fetched.

    Only the repositories the `variant` and the `uncensored` toggle actually
    need are checked and reported, and the answer ECHOES `variant` back so the
    client can tell a fresh answer from one computed for the variant it was
    showing a moment ago. Non-streaming: it costs one auth probe per repository
    plus, when all answered `ok`, one metadata listing each.
    """
    if cancel_event.is_set():
        raise Interrupted("inpaint.flux2_klein.download.check canceled before start.")
    token = read_hf_token(header)
    uncensored = bool(header.get("uncensored", False))
    result = _download_module().check_access(
        token, uncensored=uncensored, variant=_read_variant(header)
    )
    return dict(result), b""


register(METHOD_INPAINT_FLUX2_KLEIN_DOWNLOAD_CHECK, _handle_download_check)


def _handle_download_start(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """Download every missing model file, streaming two-level byte progress.

    `variant` selects both the repositories and the destination directory; an
    unknown name and the retired `variant:"4b"` (protocol 21: the application
    downloads 4B from its own model catalog) are typed refusals from the
    downloader and reach the user unchanged, like every other readable refusal
    below.

    Unlike every other method of this handler group, cancellation is observed
    INSIDE the work: `cancel_event.is_set` is handed to the downloader, which
    polls it at every chunk boundary and raises its own `DownloadCanceled`. That
    is the whole point — a cancel that only stopped the progress frames while the
    socket kept pulling gigabytes would be a lie. Files already published stay;
    the staging file of the one in flight is removed by the shared primitive.
    """
    if cancel_event.is_set():
        raise Interrupted("inpaint.flux2_klein.download.start canceled before start.")
    downloads = _download_module()
    token = read_hf_token(header)
    uncensored = bool(header.get("uncensored", False))
    try:
        result = downloads.download(
            token,
            uncensored=uncensored,
            variant=_read_variant(header),
            progress_callback=progress_forwarder(ctx, METHOD_INPAINT_FLUX2_KLEIN_DOWNLOAD_START),
            should_cancel=cancel_event.is_set,
        )
    except downloads.DownloadCanceled:
        raise Interrupted("inpaint.flux2_klein.download.start canceled.") from None
    except (ValueError, FileNotFoundError, RuntimeError):
        # Readable refusals (no token, no access, no space, changed repo layout)
        # are already user-facing text and must reach the user unchanged.
        raise
    except Exception:  # noqa: BLE001
        if cancel_event.is_set():
            raise Interrupted("inpaint.flux2_klein.download.start canceled.") from None
        traceback.print_exc()
        raise
    return dict(result), b""


register(METHOD_INPAINT_FLUX2_KLEIN_DOWNLOAD_START, _handle_download_start)
