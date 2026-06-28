"""
File: modules/ai_backend/ipc/handlers/device.py

Methods hosted here:
    device.get              — read the current device/ONNX state (METHOD_DEVICE_GET)
    device.set              — change device/ONNX selection (METHOD_DEVICE_SET)
    device.cuda_diagnostics — run CUDA/ROCm diagnostics (METHOD_DEVICE_CUDA_DIAGNOSTICS)

``device.set`` accepts any subset of the four changeable fields
(``device``, ``onnx_provider``, ``onnx_device_id``, ``max_loaded_models``);
absent/null fields are left unchanged.  Both ``device.get`` and ``device.set``
return the full new device-state shape (11 fields, identical to the HTTP path).
A ``device`` event is also pushed on each state change (handled by the event
bus, not this module).

Response fields (device.get and device.set, status=ok):
    selected_device:                 string
    available_devices:               string[]
    available_device_options:        object[]
    torch_device_needs_selection:    bool
    max_loaded_models:               int
    selected_onnx_provider:          string
    available_onnx_providers:        string[]
    selected_onnx_device_id:         string
    available_onnx_device_options:   object[]
    available_onnx_devices_by_provider: object
    onnx_device_needs_selection:     bool

Response fields (device.cuda_diagnostics, status=ok):
    diagnostics: object
"""

from __future__ import annotations

import threading
import traceback
from typing import Any

from ..protocol import (
    METHOD_DEVICE_CUDA_DIAGNOSTICS,
    METHOD_DEVICE_GET,
    METHOD_DEVICE_SET,
)
from ..registry import HandlerContext, register

# ---------------------------------------------------------------------------
# Shared helper: build the full device-state response dict from the raw dict
# returned by AiDeviceService.get_state() / set_device().  Mirrors the
# defaults used in the HTTP _handle_ai_device_get / _handle_ai_device_set.
# ---------------------------------------------------------------------------

_DEFAULT_ONNX_DEVICE_OPTIONS = [{"id": "0", "label": "0"}]
_DEFAULT_ONNX_DEVICES_BY_PROVIDER = {"CPUExecutionProvider": [{"id": "0", "label": "0"}]}


def _build_device_response(state_payload: dict[str, Any]) -> dict[str, Any]:
    """Map the raw AiDeviceService state dict to the full 11-field IPC response.

    Uses the same defaults as ``_handle_ai_device_get`` / ``_handle_ai_device_set``
    in ``server.py`` so both transports are identical.
    """
    return {
        "selected_device": state_payload.get("selected_device", "cpu"),
        "available_devices": state_payload.get("available_devices", ["cpu"]),
        "available_device_options": state_payload.get("available_device_options", []),
        "torch_device_needs_selection": bool(
            state_payload.get("torch_device_needs_selection", False)
        ),
        "max_loaded_models": state_payload.get("max_loaded_models", 3),
        "selected_onnx_provider": state_payload.get(
            "selected_onnx_provider",
            "CPUExecutionProvider",
        ),
        "available_onnx_providers": state_payload.get(
            "available_onnx_providers",
            ["CPUExecutionProvider"],
        ),
        "selected_onnx_device_id": state_payload.get("selected_onnx_device_id", "0"),
        "available_onnx_device_options": state_payload.get(
            "available_onnx_device_options",
            _DEFAULT_ONNX_DEVICE_OPTIONS,
        ),
        "available_onnx_devices_by_provider": state_payload.get(
            "available_onnx_devices_by_provider",
            _DEFAULT_ONNX_DEVICES_BY_PROVIDER,
        ),
        "onnx_device_needs_selection": bool(
            state_payload.get("onnx_device_needs_selection", False)
        ),
    }


# ---------------------------------------------------------------------------
# Handlers
# ---------------------------------------------------------------------------


def _handle_device_get(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`device.get`: return the current Torch + ONNX device state.

    No request fields; no blob in either direction.  Returns the full 11-field
    device-state dict (same shape as the HTTP GET /device response minus ``ok``).
    """
    try:
        state_payload = ctx.state.ai_device.get_state()
    except Exception as exc:
        traceback.print_exc()
        raise RuntimeError(str(exc)) from exc

    return _build_device_response(state_payload), b""


def _handle_device_set(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`device.set`: change Torch device / ONNX provider / device-id / model cap.

    Request fields (all optional; absent/null -> unchanged):
        device:           string|null
        onnx_provider:    string|null
        onnx_device_id:   string|null
        max_loaded_models: int|null

    Returns the full 11-field device-state dict after the change, identical in
    shape to ``device.get``.
    """
    try:
        state_payload = ctx.state.ai_device.set_device(
            header.get("device"),
            header.get("onnx_provider"),
            header.get("onnx_device_id"),
            header.get("max_loaded_models"),
        )
    except ValueError as exc:
        raise ValueError(str(exc)) from exc
    except Exception as exc:
        traceback.print_exc()
        raise RuntimeError(str(exc)) from exc

    return _build_device_response(state_payload), b""


def _handle_device_cuda_diagnostics(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`device.cuda_diagnostics`: run CUDA/ROCm diagnostics.

    No request fields; no blob in either direction.  Returns
    ``{"diagnostics": <object>}``.
    """
    try:
        diagnostics = ctx.state.ai_device.diagnose_cuda_rocm()
    except Exception as exc:
        traceback.print_exc()
        raise RuntimeError(str(exc)) from exc

    return {"diagnostics": diagnostics}, b""


register(METHOD_DEVICE_GET, _handle_device_get)
register(METHOD_DEVICE_SET, _handle_device_set)
register(METHOD_DEVICE_CUDA_DIAGNOSTICS, _handle_device_cuda_diagnostics)
