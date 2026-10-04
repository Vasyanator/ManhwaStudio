"""
File: modules/ai_backend/ipc/handlers/flux2_klein.py

Methods hosted here:
    inpaint.flux2_klein           — FLUX.2 klein region editing, streaming.
    inpaint.flux2_klein.status    — component availability + free memory.
    inpaint.flux2_klein.estimate  — RAM/VRAM forecast for one run.
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
The result PNG goes in the response blob (raw bytes) and its length is repeated
as ``image_len`` in the response header, next to the OOM-recovery report.
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
    METHOD_INPAINT_FLUX2_KLEIN_ESTIMATE,
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

_PROGRESS_EMITTER_ATTR = "progress_emitter"

#: The memory settings reported back as `applied`, in the order `PROTOCOL.md §5.4`
#: lists them. The Rust client deserializes the object as ONE struct and ignores an
#: incomplete one, so every name here must be present in every response.
_APPLIED_FLAGS = (
    "unload_transformer_before_vae",
    "vae_tiling",
    "vae_slicing",
    "unload_text_encoder_after_encode",
    "text_encoder_fp8",
)


def _require_segment_len(header: dict[str, Any], field: str, *, optional: bool) -> int:
    """Read one blob-segment length; an absent OPTIONAL field reads as 0."""
    value = header.get(field)
    if value is None and optional:
        return 0
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise ValueError(f"Field '{field}' must be a non-negative integer.")
    return value


def _split_request_blob(
    header: dict[str, Any], blob: bytes
) -> tuple[bytes, bytes, bytes | None]:
    """Split the request blob into region PNG + mask PNG + optional reference PNG.

    Strict equality is required: `image_len + mask_len + reference_len` must be
    exactly the blob length, so a truncated or over-long frame is a request error
    rather than a silently mis-sliced image. `reference_len` absent or `0` means
    the request carries no reference, and the third value is `None`.
    """
    image_len = _require_segment_len(header, "image_len", optional=False)
    mask_len = _require_segment_len(header, "mask_len", optional=False)
    reference_len = _require_segment_len(header, "reference_len", optional=True)
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


def _require_params(header: dict[str, Any]) -> dict[str, Any]:
    """Read the `params` object; `null`/absent means an empty object."""
    params_raw = header.get("params", {})
    if params_raw is None:
        params_raw = {}
    if not isinstance(params_raw, dict):
        raise ValueError("Field 'params' must be an object.")
    return params_raw


def _require_positive_int(header: dict[str, Any], field: str) -> int:
    """Read a required positive integer header field."""
    value = header.get(field)
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise ValueError(f"Field '{field}' must be a positive integer.")
    return value


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


def _progress_forwarder(ctx: HandlerContext) -> "Any":
    """Bind the request's `ProgressEmitter` into a service `progress_callback`.

    Shared by the generation method, by `prompt_cache.build` and by
    `download.start`: all three stream the same `{phase, step, total, label}`
    frame with an empty blob. A dead peer must never abort the work, so every
    emit failure is swallowed.

    The three keyword-only arguments are the download's SECOND progress level and
    are additive: `step`/`total` keep meaning the OVERALL level, so every existing
    single-level consumer stays correct, and a caller that passes four positional
    arguments — every service callback that existed before the download — emits
    exactly the frame it always did. A field left `None` is omitted from the
    frame; a consumer that does not know the three ignores them.
    """
    emitter = getattr(ctx, _PROGRESS_EMITTER_ATTR, None)

    def on_progress(
        phase: str,
        step: int,
        total: int,
        label: str,
        *,
        file_step: int | None = None,
        file_total: int | None = None,
        file_label: str | None = None,
    ) -> None:
        if emitter is None:
            return
        fields: dict[str, Any] = {
            "phase": str(phase),
            "step": int(step),
            "total": int(total),
            "label": str(label),
        }
        if file_step is not None:
            fields["file_step"] = int(file_step)
        if file_total is not None:
            fields["file_total"] = int(file_total)
        if file_label is not None:
            fields["file_label"] = str(file_label)
        try:
            emitter.emit(fields, b"")
        except Exception:  # noqa: BLE001 - peer gone; keep working
            pass

    return on_progress


def _handle_inpaint_flux2_klein(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    if cancel_event.is_set():
        raise Interrupted("inpaint.flux2_klein canceled before start.")

    image_png, mask_png, reference_png = _split_request_blob(header, blob)
    params_raw = _require_params(header)
    on_progress = _progress_forwarder(ctx)

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
        # ALL FIVE flags, always. The Rust side parses `applied` as one struct and
        # ignores a partial object outright (`Flux2AppliedFlags` in
        # `crates/ms-tab-cleaning/src/tools/ai_editor/engines/flux2_klein/progress.rs`), so dropping a key here does
        # not degrade the answer — it discards the whole thing, and with it the
        # OOM-recovery settings the next run was supposed to start from.
        "applied": {name: bool(applied.get(name, False)) for name in _APPLIED_FLAGS},
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
    params_raw = _require_params(header)
    return dict(ctx.state.flux2_klein_inpaint.status(params_raw or None)), b""


register(METHOD_INPAINT_FLUX2_KLEIN_STATUS, _handle_inpaint_flux2_klein_status)


def _handle_inpaint_flux2_klein_estimate(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    params_raw = _require_params(header)
    region_width = _require_positive_int(header, "region_width")
    region_height = _require_positive_int(header, "region_height")
    result = ctx.state.flux2_klein_inpaint.estimate(
        params=params_raw,
        region_width=region_width,
        region_height=region_height,
    )
    return dict(result), b""


register(METHOD_INPAINT_FLUX2_KLEIN_ESTIMATE, _handle_inpaint_flux2_klein_estimate)


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
    params_raw = _require_params(header)
    component = _require_non_empty_str(header, "component")
    action = _require_non_empty_str(header, "action")
    try:
        result = ctx.state.flux2_klein_inpaint.component_action(
            params_raw,
            component=component,
            action=action,
            progress_callback=_progress_forwarder(ctx),
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
    params_raw = _require_params(header)
    try:
        result = ctx.state.flux2_klein_inpaint.prompt_cache_build(
            params_raw, progress_callback=_progress_forwarder(ctx)
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
    params_raw = _require_params(header)
    return dict(ctx.state.flux2_klein_inpaint.prompt_cache_list(params_raw)), b""


register(METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_LIST, _handle_prompt_cache_list)


def _handle_prompt_cache_save(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """Store the cached prompt under `name`; `overwrite` must be explicit."""
    params_raw = _require_params(header)
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
    params_raw = _require_params(header)
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
    params_raw = _require_params(header)
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
    params_raw = _require_params(header)
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
def _read_hf_token(header: dict[str, Any]) -> str:
    """Read the `hf_token` request field. Absent or `null` means an empty token.

    An empty token is a legal request: `download.check` answers `no_token` for it
    without touching the network, and `download.start` refuses it with a readable
    message. The value is NEVER logged, echoed back, or interpolated into an
    error — the exception raised here deliberately names only the field.
    """
    value = header.get("hf_token")
    if value is None:
        return ""
    if not isinstance(value, str):
        raise ValueError("Field 'hf_token' must be a string.")
    return value


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
    token = _read_hf_token(header)
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
    unknown name and `variant:"4b"` together with `uncensored:true` are typed
    refusals from the downloader and reach the user unchanged, like every other
    readable refusal below.

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
    token = _read_hf_token(header)
    uncensored = bool(header.get("uncensored", False))
    try:
        result = downloads.download(
            token,
            uncensored=uncensored,
            variant=_read_variant(header),
            progress_callback=_progress_forwarder(ctx),
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
