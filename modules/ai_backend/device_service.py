"""
FILE OVERVIEW: modules/ai_backend/device_service.py
HTTP-facing AI backend settings service for Python backend settings.

Main responsibilities:
- Expose current PyTorch backend device and ONNX provider/device state.
- Expose and persist the loaded-model limit used by backend runtimes.
- Persist user selections in `UserConfig` when available.
- Detect human-readable device names for CUDA, DirectML, and MiGraphX.
- Provide CUDA/ROCm diagnostics for the Rust settings tab.
"""

from __future__ import annotations

import platform
import shutil
import subprocess
import threading
import time
from typing import Any, Optional

try:
    from ai_device import AIDevice
except Exception:
    from modules.ai_device import AIDevice

try:
    from config import UserConfig
except Exception:
    UserConfig = None

from .model_manager import LoadedModelManager, clamp_max_loaded_models


# ============================================================================
# AI DEVICE SERVICE
# ----------------------------------------------------------------------------
# Что в файле:
# - Обёртка над `modules/ai_device.py` для HTTP-микросервиса.
# - Возвращает список доступных устройств и текущее выбранное устройство.
# - Применяет выбор устройства с записью в `UserConfig` (если доступен).
# - Отдаёт текстовую диагностику CUDA/ROCm для UI вкладки настроек.
# - Все операции защищены `RLock`, чтобы backend worker-потоки не гоняли
#   состояние при параллельных запросах.
# ============================================================================


class _MemoryUserConfig:
    def __init__(self) -> None:
        self.config = {"General": {}}

    def save(self) -> None:
        return


def _elapsed_ms(started_at: float) -> int:
    return int((time.perf_counter() - started_at) * 1000)


def _provider_option_counts(
    devices_by_provider: dict[str, list[dict[str, str]]],
) -> dict[str, int]:
    return {
        provider: len(options)
        for provider, options in devices_by_provider.items()
    }


def _device_log(message: str) -> None:
    print(f"[AI Backend][device] {message}", flush=True)


class _OnnxDeviceSelector:
    PROVIDER_CONFIG_PATH = ("General", "ai_onnx_provider")
    DEVICE_ID_CONFIG_PATH = ("General", "ai_onnx_device_id")
    PROVIDER_CONFIGURED_PATH = ("General", "ai_onnx_provider_configured")
    DEVICE_ID_CONFIGURED_PATH = ("General", "ai_onnx_device_id_configured")
    DEFAULT_PROVIDER = "CPUExecutionProvider"
    DEFAULT_DEVICE_ID = "0"

    def __init__(self, user_config: Any) -> None:
        self._user_config = user_config

    def get_state(self) -> dict[str, Any]:
        started_at = time.perf_counter()
        providers = self._detect_available_providers()
        selected_provider = self._resolve_selected_provider(providers)
        devices_by_provider = self._build_devices_by_provider(providers)
        device_options = devices_by_provider.get(selected_provider, [])
        selected_device_id = self._resolve_selected_device_id(device_options)
        device_needs_selection = self._device_needs_manual_selection(
            selected_provider,
            device_options,
        )
        state = {
            "selected_onnx_provider": selected_provider,
            "available_onnx_providers": providers,
            "selected_onnx_device_id": selected_device_id,
            "available_onnx_device_options": device_options,
            "available_onnx_devices_by_provider": devices_by_provider,
            "onnx_device_needs_selection": device_needs_selection,
        }
        _device_log(
            "onnx_state "
            f"elapsed_ms={_elapsed_ms(started_at)} "
            f"provider={selected_provider!r} provider_configured={self._provider_is_configured()} "
            f"device_id={selected_device_id!r} device_configured={self._device_id_is_configured()} "
            f"needs_selection={device_needs_selection} "
            f"providers={providers!r} selected_device_options={len(device_options)} "
            f"devices_by_provider={_provider_option_counts(devices_by_provider)!r}"
        )
        return state

    def set_selection(self, raw_provider: Any, raw_device_id: Any) -> dict[str, Any]:
        _device_log(
            "set_onnx_selection_request "
            f"raw_provider={raw_provider!r} raw_device_id={raw_device_id!r}"
        )
        provider = self._normalize_provider(raw_provider)
        providers = self._detect_available_providers()
        if providers and provider not in providers:
            raise ValueError(
                f"ONNX provider '{provider}' is not available now. Available: {', '.join(providers)}"
            )

        device_options = self._build_devices_by_provider(providers).get(provider, [])
        normalized_device_id = self._normalize_device_id(raw_device_id)
        available_ids = [option["id"] for option in device_options]
        if available_ids and normalized_device_id not in available_ids:
            raise ValueError(
                f"ONNX device '{normalized_device_id}' is not available for {provider}. "
                f"Available: {', '.join(available_ids)}"
            )

        self._set_config_value(
            self.PROVIDER_CONFIG_PATH,
            provider,
            mark_configured=True,
        )
        self._set_config_value(
            self.DEVICE_ID_CONFIG_PATH,
            normalized_device_id,
            mark_configured=True,
        )
        _device_log(
            "set_onnx_selection_saved "
            f"provider={provider!r} device_id={normalized_device_id!r} "
            f"providers={providers!r} available_ids={available_ids!r}"
        )

        return self.get_state()

    def _resolve_selected_provider(self, providers: list[str]) -> str:
        configured = self._get_config_value(self.PROVIDER_CONFIG_PATH)
        provider_is_configured = self._provider_is_configured()
        if provider_is_configured and configured and configured in providers:
            return configured
        if providers:
            fallback = self._default_provider_from_available(providers)
            if provider_is_configured and configured:
                self._set_config_value(
                    self.PROVIDER_CONFIG_PATH,
                    fallback,
                    mark_configured=True,
                )
            return fallback
        return self.DEFAULT_PROVIDER

    def _resolve_selected_device_id(self, device_options: list[dict[str, str]]) -> str:
        configured = self._get_config_value(self.DEVICE_ID_CONFIG_PATH)
        device_is_configured = self._device_id_is_configured()
        available_ids = [option["id"] for option in device_options]
        if device_is_configured and configured and configured in available_ids:
            return configured
        if available_ids:
            fallback = available_ids[0]
            if device_is_configured:
                self._set_config_value(
                    self.DEVICE_ID_CONFIG_PATH,
                    fallback,
                    mark_configured=True,
                )
            return fallback
        return self.DEFAULT_DEVICE_ID

    def _default_provider_from_available(self, providers: list[str]) -> str:
        if (
            platform.system().lower() == "windows"
            and "DmlExecutionProvider" in providers
        ):
            return "DmlExecutionProvider"
        if self.DEFAULT_PROVIDER in providers:
            return self.DEFAULT_PROVIDER
        return providers[0]

    def _device_needs_manual_selection(
        self,
        selected_provider: str,
        device_options: list[dict[str, str]],
    ) -> bool:
        if selected_provider != "DmlExecutionProvider":
            return False
        if self._device_id_is_configured():
            return False
        available_ids = [
            str(option.get("id", "")).strip()
            for option in device_options
            if str(option.get("id", "")).strip()
        ]
        return len(available_ids) >= 1

    def _detect_available_providers(self) -> list[str]:
        started_at = time.perf_counter()
        try:
            import onnxruntime as ort  # type: ignore

            providers = ort.get_available_providers()
        except Exception as exc:
            _device_log(
                "detect_onnx_providers_failed "
                f"elapsed_ms={_elapsed_ms(started_at)} error={type(exc).__name__}: {exc}"
            )
            providers = []

        normalized = [
            str(provider).strip()
            for provider in providers
            if isinstance(provider, str) and str(provider).strip()
        ]
        if not normalized:
            _device_log(
                "detect_onnx_providers_empty "
                f"elapsed_ms={_elapsed_ms(started_at)} fallback={self.DEFAULT_PROVIDER!r}"
            )
            return [self.DEFAULT_PROVIDER]

        deduped: list[str] = []
        for provider in normalized:
            if provider not in deduped:
                deduped.append(provider)
        _device_log(
            "detect_onnx_providers_ok "
            f"elapsed_ms={_elapsed_ms(started_at)} providers={deduped!r}"
        )
        return deduped

    def _device_options_for_provider(self, provider: str) -> list[dict[str, str]]:
        started_at = time.perf_counter()
        names_by_id = self._detect_provider_device_names(provider)
        options: list[dict[str, str]] = []

        if names_by_id:
            for device_id, name in names_by_id.items():
                label = f"{device_id}: {name}" if name else device_id
                options.append({"id": device_id, "label": label})
            _device_log(
                "device_options_from_names "
                f"elapsed_ms={_elapsed_ms(started_at)} provider={provider!r} "
                f"ids={[option['id'] for option in options]!r}"
            )
            return options

        configured = self._get_config_value(self.DEVICE_ID_CONFIG_PATH)
        fallback_id = configured or self.DEFAULT_DEVICE_ID
        _device_log(
            "device_options_fallback "
            f"elapsed_ms={_elapsed_ms(started_at)} provider={provider!r} "
            f"configured={configured!r} fallback_id={fallback_id!r}"
        )
        return [{"id": fallback_id, "label": fallback_id}]

    def _build_devices_by_provider(
        self, providers: list[str]
    ) -> dict[str, list[dict[str, str]]]:
        mapping: dict[str, list[dict[str, str]]] = {}
        for provider in providers:
            mapping[provider] = self._device_options_for_provider(provider)
        if not mapping:
            mapping[self.DEFAULT_PROVIDER] = self._device_options_for_provider(self.DEFAULT_PROVIDER)
        return mapping

    def _detect_provider_device_names(self, provider: str) -> dict[str, str]:
        normalized = provider.strip().lower()
        if normalized == "cudaexecutionprovider":
            return self._detect_cuda_device_names()
        if normalized == "dmlexecutionprovider":
            return self._detect_directml_device_names()
        if normalized == "migraphxexecutionprovider":
            return self._detect_migraphx_device_names()
        return {}

    def _detect_cuda_device_names(self) -> dict[str, str]:
        started_at = time.perf_counter()
        names: dict[str, str] = {}
        try:
            import torch  # type: ignore

            if not hasattr(torch, "cuda") or not torch.cuda.is_available():
                _device_log(
                    "detect_cuda_names_unavailable "
                    f"elapsed_ms={_elapsed_ms(started_at)}"
                )
                return names
            count = int(torch.cuda.device_count())
            for idx in range(max(0, count)):
                device_id = str(idx)
                try:
                    name = str(torch.cuda.get_device_name(idx)).strip()
                except Exception:
                    name = ""
                names[device_id] = name or f"NVIDIA GPU {device_id}"
        except Exception as exc:
            _device_log(
                "detect_cuda_names_failed "
                f"elapsed_ms={_elapsed_ms(started_at)} error={type(exc).__name__}: {exc}"
            )
            return names
        _device_log(
            "detect_cuda_names_ok "
            f"elapsed_ms={_elapsed_ms(started_at)} ids={list(names.keys())!r}"
        )
        return names

    def _detect_directml_device_names(self) -> dict[str, str]:
        started_at = time.perf_counter()
        if platform.system().lower() != "windows":
            _device_log("detect_directml_names_skipped_non_windows")
            return {}

        device_names: list[str] = []
        try:
            import torch_directml  # type: ignore

            count = int(torch_directml.device_count())
            _device_log(
                "detect_directml_torch_directml_count "
                f"elapsed_ms={_elapsed_ms(started_at)} count={count}"
            )
            for idx in range(max(0, count)):
                name = ""
                get_name = getattr(torch_directml, "device_name", None)
                if callable(get_name):
                    try:
                        name = str(get_name(idx)).strip()
                    except Exception:
                        name = ""
                if not name:
                    name = self._detect_windows_gpu_names_by_order().get(str(idx), "")
                device_names.append(name or f"DirectML GPU {idx}")
        except Exception as exc:
            _device_log(
                "detect_directml_torch_directml_failed "
                f"elapsed_ms={_elapsed_ms(started_at)} error={type(exc).__name__}: {exc}"
            )
            gpu_names = self._detect_windows_gpu_names_by_order()
            result = {
                device_id: name or f"DirectML GPU {device_id}"
                for device_id, name in gpu_names.items()
            }
            _device_log(
                "detect_directml_names_windows_fallback "
                f"elapsed_ms={_elapsed_ms(started_at)} ids={list(result.keys())!r}"
            )
            return result

        result = {
            str(idx): device_names[idx]
            for idx in range(len(device_names))
        }
        _device_log(
            "detect_directml_names_ok "
            f"elapsed_ms={_elapsed_ms(started_at)} ids={list(result.keys())!r}"
        )
        return result

    def _detect_migraphx_device_names(self) -> dict[str, str]:
        names = self._detect_rocm_gpu_names_by_order()
        return {
            device_id: name or f"AMD GPU {device_id}"
            for device_id, name in names.items()
        }

    def _detect_windows_gpu_names_by_order(self) -> dict[str, str]:
        started_at = time.perf_counter()
        result = self._detect_windows_gpu_names_powershell()
        if not result:
            result = self._detect_windows_gpu_names_wmic()
        if not result:
            _device_log(
                "detect_windows_gpu_names_empty "
                f"elapsed_ms={_elapsed_ms(started_at)}"
            )
            return {}

        names = [line.strip() for line in result.splitlines() if line.strip()]
        mapped = {str(idx): name for idx, name in enumerate(names)}
        _device_log(
            "detect_windows_gpu_names_ok "
            f"elapsed_ms={_elapsed_ms(started_at)} ids={list(mapped.keys())!r}"
        )
        return mapped

    def _detect_windows_gpu_names_powershell(self) -> str:
        command = shutil.which("powershell") or shutil.which("pwsh")
        if command is None:
            _device_log("windows_gpu_names_powershell_missing")
            return ""

        query = (
            "Get-CimInstance Win32_VideoController | "
            "Select-Object -ExpandProperty Name"
        )
        result = self._run_command(
            [command, "-NoProfile", "-Command", query],
            timeout=1.0,
        )
        _device_log(
            "windows_gpu_names_powershell_result "
            f"command={command!r} lines={len(result.splitlines()) if result else 0}"
        )
        return result

    def _detect_windows_gpu_names_wmic(self) -> str:
        command = shutil.which("wmic")
        if command is None:
            _device_log("windows_gpu_names_wmic_missing")
            return ""

        result = self._run_command(
            [command, "path", "win32_VideoController", "get", "Name"],
            timeout=1.0,
        )
        if not result:
            _device_log("windows_gpu_names_wmic_empty")
            return ""

        filtered = "\n".join(
            line.strip()
            for line in result.splitlines()
            if line.strip() and line.strip().lower() != "name"
        )
        _device_log(
            "windows_gpu_names_wmic_result "
            f"lines={len(filtered.splitlines()) if filtered else 0}"
        )
        return filtered

    def _detect_rocm_gpu_names_by_order(self) -> dict[str, str]:
        command = shutil.which("rocm-smi")
        if command is None:
            return {}

        result = self._run_command([command, "--showproductname"])
        if not result:
            return {}

        names: dict[str, str] = {}
        for raw_line in result.splitlines():
            line = raw_line.strip()
            if not line:
                continue
            if not line.lower().startswith("gpu["):
                continue
            prefix, _, name = line.partition(":")
            start = prefix.find("[")
            end = prefix.find("]")
            if start < 0 or end <= start + 1:
                continue
            device_id = prefix[start + 1:end].strip()
            if not device_id:
                continue
            names[device_id] = name.strip() or f"AMD GPU {device_id}"
        return names

    def _run_command(self, cmd: list[str], timeout: float = 1.0) -> str:
        started_at = time.perf_counter()
        try:
            completed = subprocess.run(
                cmd,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                timeout=timeout,
                check=False,
            )
        except Exception as exc:
            _device_log(
                "run_command_failed "
                f"elapsed_ms={_elapsed_ms(started_at)} cmd={cmd[:2]!r} "
                f"error={type(exc).__name__}: {exc}"
            )
            return ""
        if completed.returncode != 0:
            _device_log(
                "run_command_nonzero "
                f"elapsed_ms={_elapsed_ms(started_at)} cmd={cmd[:2]!r} "
                f"code={completed.returncode} stderr_len={len(completed.stderr or '')}"
            )
            return ""
        _device_log(
            "run_command_ok "
            f"elapsed_ms={_elapsed_ms(started_at)} cmd={cmd[:2]!r} "
            f"stdout_len={len(completed.stdout or '')}"
        )
        return completed.stdout.strip()

    def _get_config_value(self, path: tuple[str, ...]) -> Optional[str]:
        text = self._get_raw_config_text(path)
        if text is None:
            return None
        if text.lower() == "not-selected":
            return None
        return text or None

    def _get_raw_config_text(self, path: tuple[str, ...]) -> Optional[str]:
        node = getattr(self._user_config, "config", None)
        if not isinstance(node, dict):
            return None

        current: Any = node
        for key in path:
            if not isinstance(current, dict):
                return None
            current = current.get(key)

        if isinstance(current, (int, str)):
            text = str(current).strip()
            return text or None
        return None

    def _provider_is_configured(self) -> bool:
        raw_configured = self._get_raw_config_text(self.PROVIDER_CONFIG_PATH)
        if raw_configured is not None and raw_configured.strip().lower() == "not-selected":
            return False
        if self._get_bool_config_value(self.PROVIDER_CONFIGURED_PATH):
            return True
        configured = self._get_config_value(self.PROVIDER_CONFIG_PATH)
        return configured not in {None, self.DEFAULT_PROVIDER}

    def _device_id_is_configured(self) -> bool:
        raw_configured = self._get_raw_config_text(self.DEVICE_ID_CONFIG_PATH)
        if raw_configured is not None and raw_configured.strip().lower() == "not-selected":
            return False
        if self._get_bool_config_value(self.DEVICE_ID_CONFIGURED_PATH):
            return True
        configured = self._get_config_value(self.DEVICE_ID_CONFIG_PATH)
        return configured not in {None, self.DEFAULT_DEVICE_ID}

    def _get_bool_config_value(self, path: tuple[str, ...]) -> bool:
        node = getattr(self._user_config, "config", None)
        if not isinstance(node, dict):
            return False

        current: Any = node
        for key in path:
            if not isinstance(current, dict):
                return False
            current = current.get(key)

        return bool(current) if isinstance(current, bool) else False

    def _set_config_value(
        self,
        path: tuple[str, ...],
        value: str,
        *,
        mark_configured: bool = False,
    ) -> None:
        node = getattr(self._user_config, "config", None)
        if not isinstance(node, dict):
            raise TypeError("user_config must provide dict-like 'config' attribute")

        current = node
        for key in path[:-1]:
            nested = current.get(key)
            if not isinstance(nested, dict):
                nested = {}
                current[key] = nested
            current = nested
        current[path[-1]] = value
        if mark_configured and path == self.PROVIDER_CONFIG_PATH:
            current[self.PROVIDER_CONFIGURED_PATH[-1]] = True
        if mark_configured and path == self.DEVICE_ID_CONFIG_PATH:
            current[self.DEVICE_ID_CONFIGURED_PATH[-1]] = True

        save = getattr(self._user_config, "save", None)
        if callable(save):
            save()

    def _normalize_provider(self, value: Any) -> str:
        normalized = str(value or "").strip()
        if not normalized:
            raise ValueError("Field 'onnx_provider' must be a non-empty string.")
        return normalized

    def _normalize_device_id(self, value: Any) -> str:
        normalized = str(value if value is not None else self.DEFAULT_DEVICE_ID).strip()
        if not normalized:
            return self.DEFAULT_DEVICE_ID
        return normalized


class AiDeviceService:
    MAX_LOADED_MODELS_CONFIG_PATH = ("General", "ai_max_loaded_models")

    def __init__(self, model_manager: LoadedModelManager) -> None:
        self._lock = threading.RLock()
        self._user_config = UserConfig if UserConfig is not None else _MemoryUserConfig()
        self._model_manager = model_manager
        self._selector = AIDevice(self._user_config)
        self._onnx_selector = _OnnxDeviceSelector(self._user_config)
        self._selected_device = str(self._selector)
        self._ensure_model_limit_config_locked()

    def get_state(self) -> dict[str, Any]:
        with self._lock:
            started_at = time.perf_counter()
            available = self._available_devices_locked()
            selected = self._resolve_selected_locked(available)
            options = self._build_device_options_locked(available)
            max_loaded_models = self._get_max_loaded_models_locked()
            payload = {
                "selected_device": selected,
                "available_devices": available,
                "available_device_options": options,
                "torch_device_needs_selection": AIDevice.needs_manual_selection(
                    self._user_config,
                    available,
                ),
                "max_loaded_models": max_loaded_models,
            }
            payload.update(self._onnx_selector.get_state())
            _device_log(
                "state_response "
                f"elapsed_ms={_elapsed_ms(started_at)} "
                f"torch_selected={selected!r} torch_available={available!r} "
                f"torch_options={len(options)} "
                f"torch_needs_selection={payload['torch_device_needs_selection']} "
                f"onnx_provider={payload.get('selected_onnx_provider')!r} "
                f"onnx_providers={payload.get('available_onnx_providers')!r} "
                f"onnx_device={payload.get('selected_onnx_device_id')!r} "
                f"onnx_options={len(payload.get('available_onnx_device_options') or [])} "
                f"onnx_needs_selection={payload.get('onnx_device_needs_selection')}"
            )
            return payload

    def set_device(
        self,
        raw_device: Any,
        raw_onnx_provider: Any = None,
        raw_onnx_device_id: Any = None,
        raw_max_loaded_models: Any = None,
    ) -> dict[str, Any]:
        with self._lock:
            started_at = time.perf_counter()
            _device_log(
                "set_device_request "
                f"raw_device={raw_device!r} raw_onnx_provider={raw_onnx_provider!r} "
                f"raw_onnx_device_id={raw_onnx_device_id!r} "
                f"raw_max_loaded_models={raw_max_loaded_models!r}"
            )
            if raw_device is not None:
                if not isinstance(raw_device, str) or not raw_device.strip():
                    raise ValueError("Field 'device' must be a non-empty string.")
                selected = self._selector.change_device(raw_device)
                self._selected_device = selected
                _device_log(f"set_torch_device_saved selected={selected!r}")

            if raw_onnx_provider is not None or raw_onnx_device_id is not None:
                current = self._onnx_selector.get_state()
                provider_value = (
                    raw_onnx_provider
                    if raw_onnx_provider is not None
                    else current["selected_onnx_provider"]
                )
                device_id_value = (
                    raw_onnx_device_id
                    if raw_onnx_device_id is not None
                    else current["selected_onnx_device_id"]
                )
                self._onnx_selector.set_selection(provider_value, device_id_value)

            if raw_max_loaded_models is not None:
                max_loaded_models = clamp_max_loaded_models(raw_max_loaded_models)
                self._set_config_value(self.MAX_LOADED_MODELS_CONFIG_PATH, str(max_loaded_models))
                self._model_manager.set_max_loaded_models(max_loaded_models)
                _device_log(f"set_max_loaded_models_saved value={max_loaded_models}")

            available = self._available_devices_locked()
            options = self._build_device_options_locked(available)
            payload = {
                "selected_device": self._resolve_selected_locked(available),
                "available_devices": available,
                "available_device_options": options,
                "torch_device_needs_selection": AIDevice.needs_manual_selection(
                    self._user_config,
                    available,
                ),
                "max_loaded_models": self._get_max_loaded_models_locked(),
            }
            payload.update(self._onnx_selector.get_state())
            _device_log(
                "set_device_response "
                f"elapsed_ms={_elapsed_ms(started_at)} "
                f"torch_selected={payload['selected_device']!r} "
                f"torch_needs_selection={payload['torch_device_needs_selection']} "
                f"onnx_provider={payload.get('selected_onnx_provider')!r} "
                f"onnx_device={payload.get('selected_onnx_device_id')!r} "
                f"onnx_needs_selection={payload.get('onnx_device_needs_selection')}"
            )
            return payload

    def diagnose_cuda_rocm(self) -> str:
        with self._lock:
            return AIDevice.diagnose_cuda_rocm()

    def _available_devices_locked(self) -> list[str]:
        available = AIDevice.detect_available_devices()
        if not available:
            return ["cpu"]
        return available

    def _resolve_selected_locked(self, available: list[str]) -> str:
        configured = AIDevice._get_config_value(self._user_config)
        if configured and configured in available:
            self._selected_device = configured
            return self._selected_device

        if configured is not None and self._selected_device in available:
            return self._selected_device

        refreshed = str(AIDevice(self._user_config))
        if refreshed in available:
            self._selected_device = refreshed
        elif configured is None:
            self._selected_device = AIDevice._default_device_from_available(available)
        else:
            self._selected_device = available[0]
        return self._selected_device

    def _build_device_options_locked(self, available: list[str]) -> list[dict[str, str]]:
        cuda_names = self._detect_cuda_device_names_locked()
        options: list[dict[str, str]] = []

        for device in available:
            label = device
            if device == "cpu":
                label = "CPU (cpu)"
            elif device == "mps":
                label = "Apple Metal (mps)"
            elif device == "cuda":
                first_name = next(iter(cuda_names.values()), "NVIDIA GPU")
                label = f"Авто: {first_name} (cuda)"
            elif device.startswith("cuda:"):
                name = cuda_names.get(device)
                if name:
                    label = f"{name} ({device})"
            options.append({"id": device, "label": label})

        return options

    def _detect_cuda_device_names_locked(self) -> dict[str, str]:
        names: dict[str, str] = {}
        try:
            import torch  # type: ignore

            if not hasattr(torch, "cuda") or not torch.cuda.is_available():
                return names
            count = int(torch.cuda.device_count())
            for idx in range(max(0, count)):
                key = f"cuda:{idx}"
                try:
                    name = str(torch.cuda.get_device_name(idx)).strip()
                except Exception:
                    name = ""
                names[key] = name or f"NVIDIA GPU {idx}"
        except Exception:
            return names
        return names

    def _get_max_loaded_models_locked(self) -> int:
        configured = self._get_config_value(self.MAX_LOADED_MODELS_CONFIG_PATH)
        normalized = clamp_max_loaded_models(configured)
        current = self._model_manager.get_max_loaded_models()
        if current != normalized:
            self._model_manager.set_max_loaded_models(normalized)
        return normalized

    def _ensure_model_limit_config_locked(self) -> None:
        normalized = clamp_max_loaded_models(
            self._get_config_value(self.MAX_LOADED_MODELS_CONFIG_PATH)
        )
        self._set_config_value(self.MAX_LOADED_MODELS_CONFIG_PATH, str(normalized))
        self._model_manager.set_max_loaded_models(normalized)

    def _get_config_value(self, path: tuple[str, ...]) -> Optional[str]:
        node = getattr(self._user_config, "config", None)
        if not isinstance(node, dict):
            return None

        current: Any = node
        for key in path:
            if not isinstance(current, dict):
                return None
            current = current.get(key)

        if isinstance(current, (int, str)):
            text = str(current).strip()
            return text or None
        return None

    def _set_config_value(self, path: tuple[str, ...], value: str) -> None:
        node = getattr(self._user_config, "config", None)
        if not isinstance(node, dict):
            raise TypeError("user_config must provide dict-like 'config' attribute")

        current = node
        for key in path[:-1]:
            nested = current.get(key)
            if not isinstance(nested, dict):
                nested = {}
                current[key] = nested
            current = nested
        current[path[-1]] = value

        save = getattr(self._user_config, "save", None)
        if callable(save):
            save()
