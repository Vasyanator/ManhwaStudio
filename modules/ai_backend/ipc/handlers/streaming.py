"""
File: modules/ai_backend/ipc/handlers/streaming.py

Purpose:
Request-reading and progress-streaming helpers shared by the handler groups of
the region editors (`flux2_klein.py`, `flux3_klein.py`). Registers NO method:
it is a helper module, not a handler group, so `__init__.py` does not import it.

Key functions:
- `require_segment_len()` — one blob-segment length field (strict, non-negative);
- `require_params()` — the `params` object (`null` / absent = `{}`);
- `progress_forwarder()` — the request's `ProgressEmitter` as a service
  `progress_callback` (`{phase, step, total, label}` plus the optional
  `file_step` / `file_total` / `file_label` / `file_component` and `stage`; a dead peer never
  aborts the work, the first failure is logged once);
- `read_hf_token()` — the `hf_token` field (absent = empty; never logged).
"""

from __future__ import annotations

import logging
from typing import Any

from ..registry import HandlerContext

log = logging.getLogger(__name__)

#: The `HandlerContext` attribute the dispatcher sets for a streaming method.
PROGRESS_EMITTER_ATTR = "progress_emitter"


def require_segment_len(header: dict[str, Any], field: str, *, optional: bool) -> int:
    """Read one blob-segment length; an absent OPTIONAL field reads as 0."""
    value = header.get(field)
    if value is None and optional:
        return 0
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise ValueError(f"Field '{field}' must be a non-negative integer.")
    return value


def require_params(header: dict[str, Any]) -> dict[str, Any]:
    """Read the `params` object; `null`/absent means an empty object."""
    params_raw = header.get("params", {})
    if params_raw is None:
        params_raw = {}
    if not isinstance(params_raw, dict):
        raise ValueError("Field 'params' must be an object.")
    return params_raw


def progress_forwarder(ctx: HandlerContext, method: str) -> "Any":
    """Bind the request's `ProgressEmitter` into a service `progress_callback` for `method`.

    Shared by every streaming method of the FLUX handler groups (generation,
    `component_action`, `prompt_cache.build`, `download.start`): all stream the
    same `{phase, step, total, label}` frame with an empty blob. A dead peer must never abort the work (a download
    or a run outlives one client connection), so an emit failure never reaches
    the service: the FIRST one of the request is logged at WARNING with the
    method and the frame (later frames are still attempted; their failures are
    not logged again). A disconnect therefore does not cancel a FLUX.2 run;
    the `cancel` frame is observed only before the service call and after it
    returns (mid-run cancellation is a separate, future feature).

    The three keyword-only arguments are the download's SECOND progress level and
    are additive: `step`/`total` keep meaning the OVERALL level, so every existing
    single-level consumer stays correct, and a caller that passes four positional
    arguments — every service callback that existed before the download — emits
    exactly the frame it always did. A field left `None` is omitted from the
    frame; a consumer that does not know the three ignores them. `stage` is
    likewise optional: a stable machine id of the step (FLUX.3 klein's
    `region.progress_stages()`) the client localizes, omitted when `None`.
    `file_component` (v21, optional) names WHICH component a load-phase byte
    level reads (FLUX.2 klein: `"transformer"` / `"text_encoder"`; absent
    means the transformer to the client), omitted when `None`.
    """
    emitter = getattr(ctx, PROGRESS_EMITTER_ATTR, None)
    failed = False

    def on_progress(
        phase: str,
        step: int,
        total: int,
        label: str,
        *,
        file_step: int | None = None,
        file_total: int | None = None,
        file_label: str | None = None,
        file_component: str | None = None,
        stage: str | None = None,
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
        if file_component is not None:
            fields["file_component"] = str(file_component)
        if stage is not None:
            fields["stage"] = str(stage)
        nonlocal failed
        try:
            emitter.emit(fields, b"")
        except Exception as exc:  # noqa: BLE001 - a write failure means the peer is gone; the work goes on (docstring)
            if not failed:
                failed = True
                log.warning("%s: progress frame not delivered (%s: %s), phase=%s step=%s/%s; the work continues, later failures of this request are not logged again", method, type(exc).__name__, exc, fields["phase"], fields["step"], fields["total"])

    return on_progress


def read_hf_token(header: dict[str, Any]) -> str:
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
