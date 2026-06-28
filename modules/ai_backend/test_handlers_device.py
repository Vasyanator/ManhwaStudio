"""
File: modules/ai_backend/test_handlers_device.py

Unit tests for the device IPC handler group
(modules/ai_backend/ipc/handlers/device.py).

Covered:
    device.get              — calls ai_device.get_state(), maps to full 11-field dict.
    device.set              — reads device/onnx_provider/onnx_device_id/max_loaded_models
                              from header, calls ai_device.set_device(), same 11-field
                              response.
    device.cuda_diagnostics — calls ai_device.diagnose_cuda_rocm(), returns
                              {"diagnostics": ...}.

All service calls are mocked; no torch or hardware is needed.
"""

from __future__ import annotations

import threading
from types import SimpleNamespace
from unittest.mock import MagicMock

import pytest

from modules.ai_backend.ipc.handlers import device as device_mod
from modules.ai_backend.ipc.protocol import (
    METHOD_DEVICE_CUDA_DIAGNOSTICS,
    METHOD_DEVICE_GET,
    METHOD_DEVICE_SET,
)
from modules.ai_backend.ipc.registry import HandlerContext, METHOD_HANDLERS

# ---------------------------------------------------------------------------
# The 11 keys that MUST appear in every device.get / device.set response.
# ---------------------------------------------------------------------------
DEVICE_RESPONSE_KEYS = {
    "selected_device",
    "available_devices",
    "available_device_options",
    "torch_device_needs_selection",
    "max_loaded_models",
    "selected_onnx_provider",
    "available_onnx_providers",
    "selected_onnx_device_id",
    "available_onnx_device_options",
    "available_onnx_devices_by_provider",
    "onnx_device_needs_selection",
}

# A realistic AiDeviceService state dict (as returned by get_state / set_device).
_FULL_STATE = {
    "selected_device": "cuda",
    "available_devices": ["cpu", "cuda"],
    "available_device_options": [{"id": "cpu", "label": "CPU"}, {"id": "cuda", "label": "CUDA 0"}],
    "torch_device_needs_selection": False,
    "max_loaded_models": 3,
    "selected_onnx_provider": "CUDAExecutionProvider",
    "available_onnx_providers": ["CPUExecutionProvider", "CUDAExecutionProvider"],
    "selected_onnx_device_id": "0",
    "available_onnx_device_options": [{"id": "0", "label": "0"}],
    "available_onnx_devices_by_provider": {
        "CPUExecutionProvider": [{"id": "0", "label": "0"}],
        "CUDAExecutionProvider": [{"id": "0", "label": "0"}],
    },
    "onnx_device_needs_selection": False,
}


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _make_ctx(ai_device_svc=None):
    state = SimpleNamespace(ai_device=ai_device_svc or MagicMock())
    return HandlerContext(
        state=state,
        events=MagicMock(),
        get_health_snapshot=lambda: {},
    )


def _call(handler_fn, ctx, header, blob=b""):
    return handler_fn(ctx, header, blob, threading.Event())


# ---------------------------------------------------------------------------
# Registration
# ---------------------------------------------------------------------------

def test_device_get_is_registered():
    assert METHOD_DEVICE_GET in METHOD_HANDLERS


def test_device_set_is_registered():
    assert METHOD_DEVICE_SET in METHOD_HANDLERS


def test_device_cuda_diagnostics_is_registered():
    assert METHOD_DEVICE_CUDA_DIAGNOSTICS in METHOD_HANDLERS


# ---------------------------------------------------------------------------
# Helper: _build_device_response
# ---------------------------------------------------------------------------

class TestBuildDeviceResponse:
    """Tests for the shared _build_device_response helper."""

    def test_all_11_keys_present_with_full_state(self):
        resp = device_mod._build_device_response(_FULL_STATE)
        assert set(resp.keys()) == DEVICE_RESPONSE_KEYS

    def test_values_match_full_state(self):
        resp = device_mod._build_device_response(_FULL_STATE)
        assert resp["selected_device"] == "cuda"
        assert resp["available_devices"] == ["cpu", "cuda"]
        assert resp["max_loaded_models"] == 3
        assert resp["selected_onnx_provider"] == "CUDAExecutionProvider"
        assert resp["selected_onnx_device_id"] == "0"
        assert resp["onnx_device_needs_selection"] is False
        assert resp["torch_device_needs_selection"] is False

    def test_defaults_applied_for_empty_state(self):
        resp = device_mod._build_device_response({})
        assert resp["selected_device"] == "cpu"
        assert resp["available_devices"] == ["cpu"]
        assert resp["available_device_options"] == []
        assert resp["torch_device_needs_selection"] is False
        assert resp["max_loaded_models"] == 3
        assert resp["selected_onnx_provider"] == "CPUExecutionProvider"
        assert resp["available_onnx_providers"] == ["CPUExecutionProvider"]
        assert resp["selected_onnx_device_id"] == "0"
        assert resp["available_onnx_device_options"] == [{"id": "0", "label": "0"}]
        assert resp["available_onnx_devices_by_provider"] == {
            "CPUExecutionProvider": [{"id": "0", "label": "0"}]
        }
        assert resp["onnx_device_needs_selection"] is False

    def test_torch_device_needs_selection_coerced_to_bool(self):
        resp = device_mod._build_device_response({"torch_device_needs_selection": 1})
        assert resp["torch_device_needs_selection"] is True

    def test_onnx_device_needs_selection_coerced_to_bool(self):
        resp = device_mod._build_device_response({"onnx_device_needs_selection": 0})
        assert resp["onnx_device_needs_selection"] is False


# ---------------------------------------------------------------------------
# device.get
# ---------------------------------------------------------------------------

class TestDeviceGet:
    def test_calls_get_state(self):
        svc = MagicMock()
        svc.get_state.return_value = dict(_FULL_STATE)
        ctx = _make_ctx(svc)

        _call(METHOD_HANDLERS[METHOD_DEVICE_GET], ctx, {})

        svc.get_state.assert_called_once_with()

    def test_response_has_all_11_keys(self):
        svc = MagicMock()
        svc.get_state.return_value = dict(_FULL_STATE)
        ctx = _make_ctx(svc)

        resp, blob = _call(METHOD_HANDLERS[METHOD_DEVICE_GET], ctx, {})

        assert set(resp.keys()) == DEVICE_RESPONSE_KEYS
        assert blob == b""

    def test_response_values_correct(self):
        svc = MagicMock()
        svc.get_state.return_value = dict(_FULL_STATE)
        ctx = _make_ctx(svc)

        resp, _ = _call(METHOD_HANDLERS[METHOD_DEVICE_GET], ctx, {})

        assert resp["selected_device"] == "cuda"
        assert resp["available_devices"] == ["cpu", "cuda"]
        assert resp["max_loaded_models"] == 3

    def test_defaults_when_service_returns_empty(self):
        svc = MagicMock()
        svc.get_state.return_value = {}
        ctx = _make_ctx(svc)

        resp, _ = _call(METHOD_HANDLERS[METHOD_DEVICE_GET], ctx, {})

        assert resp["selected_device"] == "cpu"
        assert resp["selected_onnx_provider"] == "CPUExecutionProvider"

    def test_blob_is_empty(self):
        svc = MagicMock()
        svc.get_state.return_value = {}
        ctx = _make_ctx(svc)

        _, blob = _call(METHOD_HANDLERS[METHOD_DEVICE_GET], ctx, {})

        assert blob == b""

    def test_service_error_is_raised(self):
        svc = MagicMock()
        svc.get_state.side_effect = RuntimeError("hw error")
        ctx = _make_ctx(svc)

        with pytest.raises(RuntimeError, match="hw error"):
            _call(METHOD_HANDLERS[METHOD_DEVICE_GET], ctx, {})

    def test_no_ok_key_in_response(self):
        """The IPC layer adds status; the handler must NOT add 'ok'."""
        svc = MagicMock()
        svc.get_state.return_value = {}
        ctx = _make_ctx(svc)

        resp, _ = _call(METHOD_HANDLERS[METHOD_DEVICE_GET], ctx, {})

        assert "ok" not in resp


# ---------------------------------------------------------------------------
# device.set
# ---------------------------------------------------------------------------

class TestDeviceSet:
    def _svc(self, result=None):
        svc = MagicMock()
        svc.set_device.return_value = result or dict(_FULL_STATE)
        return svc

    def test_calls_set_device_with_all_four_fields(self):
        svc = self._svc()
        ctx = _make_ctx(svc)
        header = {
            "device": "cuda",
            "onnx_provider": "CUDAExecutionProvider",
            "onnx_device_id": "1",
            "max_loaded_models": 5,
        }

        _call(METHOD_HANDLERS[METHOD_DEVICE_SET], ctx, header)

        svc.set_device.assert_called_once_with("cuda", "CUDAExecutionProvider", "1", 5)

    def test_absent_fields_passed_as_none(self):
        svc = self._svc()
        ctx = _make_ctx(svc)

        _call(METHOD_HANDLERS[METHOD_DEVICE_SET], ctx, {})

        svc.set_device.assert_called_once_with(None, None, None, None)

    def test_null_fields_passed_as_none(self):
        svc = self._svc()
        ctx = _make_ctx(svc)
        header = {
            "device": None,
            "onnx_provider": None,
            "onnx_device_id": None,
            "max_loaded_models": None,
        }

        _call(METHOD_HANDLERS[METHOD_DEVICE_SET], ctx, header)

        svc.set_device.assert_called_once_with(None, None, None, None)

    def test_response_has_all_11_keys(self):
        svc = self._svc()
        ctx = _make_ctx(svc)

        resp, blob = _call(METHOD_HANDLERS[METHOD_DEVICE_SET], ctx, {})

        assert set(resp.keys()) == DEVICE_RESPONSE_KEYS
        assert blob == b""

    def test_partial_header_only_device(self):
        svc = self._svc()
        ctx = _make_ctx(svc)

        _call(METHOD_HANDLERS[METHOD_DEVICE_SET], ctx, {"device": "cpu"})

        svc.set_device.assert_called_once_with("cpu", None, None, None)

    def test_service_value_error_re_raised(self):
        svc = MagicMock()
        svc.set_device.side_effect = ValueError("unknown device: bogus")
        ctx = _make_ctx(svc)

        with pytest.raises(ValueError, match="unknown device: bogus"):
            _call(METHOD_HANDLERS[METHOD_DEVICE_SET], ctx, {"device": "bogus"})

    def test_service_runtime_error_re_raised(self):
        svc = MagicMock()
        svc.set_device.side_effect = RuntimeError("driver error")
        ctx = _make_ctx(svc)

        with pytest.raises(RuntimeError, match="driver error"):
            _call(METHOD_HANDLERS[METHOD_DEVICE_SET], ctx, {})

    def test_no_ok_key_in_response(self):
        svc = self._svc()
        ctx = _make_ctx(svc)

        resp, _ = _call(METHOD_HANDLERS[METHOD_DEVICE_SET], ctx, {})

        assert "ok" not in resp

    def test_blob_is_empty(self):
        svc = self._svc()
        ctx = _make_ctx(svc)

        _, blob = _call(METHOD_HANDLERS[METHOD_DEVICE_SET], ctx, {})

        assert blob == b""


# ---------------------------------------------------------------------------
# device.cuda_diagnostics
# ---------------------------------------------------------------------------

class TestDeviceCudaDiagnostics:
    def test_calls_diagnose_cuda_rocm(self):
        svc = MagicMock()
        svc.diagnose_cuda_rocm.return_value = {"cuda": True, "version": "12.0"}
        ctx = _make_ctx(svc)

        _call(METHOD_HANDLERS[METHOD_DEVICE_CUDA_DIAGNOSTICS], ctx, {})

        svc.diagnose_cuda_rocm.assert_called_once_with()

    def test_response_contains_diagnostics_key(self):
        svc = MagicMock()
        diag = {"cuda": True, "version": "12.0", "driver": "555.58"}
        svc.diagnose_cuda_rocm.return_value = diag
        ctx = _make_ctx(svc)

        resp, blob = _call(METHOD_HANDLERS[METHOD_DEVICE_CUDA_DIAGNOSTICS], ctx, {})

        assert "diagnostics" in resp
        assert resp["diagnostics"] == diag
        assert blob == b""

    def test_blob_is_empty(self):
        svc = MagicMock()
        svc.diagnose_cuda_rocm.return_value = {}
        ctx = _make_ctx(svc)

        _, blob = _call(METHOD_HANDLERS[METHOD_DEVICE_CUDA_DIAGNOSTICS], ctx, {})

        assert blob == b""

    def test_service_error_is_raised(self):
        svc = MagicMock()
        svc.diagnose_cuda_rocm.side_effect = RuntimeError("driver unavailable")
        ctx = _make_ctx(svc)

        with pytest.raises(RuntimeError, match="driver unavailable"):
            _call(METHOD_HANDLERS[METHOD_DEVICE_CUDA_DIAGNOSTICS], ctx, {})

    def test_no_ok_key_in_response(self):
        svc = MagicMock()
        svc.diagnose_cuda_rocm.return_value = {}
        ctx = _make_ctx(svc)

        resp, _ = _call(METHOD_HANDLERS[METHOD_DEVICE_CUDA_DIAGNOSTICS], ctx, {})

        assert "ok" not in resp
