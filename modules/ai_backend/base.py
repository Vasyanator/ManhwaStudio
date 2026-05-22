"""
Compatibility base utilities for ai_backend textdetector modules.

This mirrors the lightweight BaseModule API used by the migrated CTD code.
"""
from __future__ import annotations

import gc
import logging
import os
from collections import OrderedDict
from copy import deepcopy
from typing import Callable, Dict, List, Union

try:
    import torch
except Exception:  # pragma: no cover - optional dependency for CTD runtime
    torch = None  # type: ignore[assignment]

logger = logging.getLogger(__name__)

GPUINTENSIVE_SET = {"cuda", "mps", "xpu", "privateuseone"}

os.environ["PYTORCH_ENABLE_MPS_FALLBACK"] = "1"

DEFAULT_DEVICE = "cpu"
AVAILABLE_DEVICES = ["cpu"]

if torch is not None and hasattr(torch, "cuda") and torch.cuda.is_available():
    DEFAULT_DEVICE = "cuda"
    AVAILABLE_DEVICES.append(DEFAULT_DEVICE)
if torch is not None and hasattr(torch, "xpu") and torch.xpu.is_available():
    DEFAULT_DEVICE = "xpu" if torch.xpu.is_available() else "cpu"
    AVAILABLE_DEVICES.append(DEFAULT_DEVICE)
if (
    torch is not None
    and hasattr(torch, "backends")
    and hasattr(torch.backends, "mps")
    and torch.backends.mps.is_available()
):
    DEFAULT_DEVICE = "mps"
    AVAILABLE_DEVICES.append(DEFAULT_DEVICE)

try:
    import torch_directml  # type: ignore

    if (
        torch is not None
        and hasattr(torch, "privateuseone")
        and torch_directml.device_count() > 0
    ):
        torch.dml = torch_directml  # type: ignore[attr-defined]
        DEFAULT_DEVICE = f"privateuseone:{torch.dml.default_device()}"  # type: ignore[attr-defined]
        AVAILABLE_DEVICES += [f"privateuseone:{d}" for d in range(torch.dml.device_count())]  # type: ignore[attr-defined]
except Exception:
    pass


def DEVICE_SELECTOR(not_supported: List[str] | None = None):
    not_supported = not_supported or []
    return deepcopy(
        {
            "type": "selector",
            "options": [
                opt
                for opt in AVAILABLE_DEVICES
                if all(device not in opt for device in not_supported)
            ],
            "value": DEFAULT_DEVICE
            if not any(DEFAULT_DEVICE in device for device in not_supported)
            else "cpu",
        }
    )


def soft_empty_cache():
    if torch is None:
        return
    gc.collect()
    if DEFAULT_DEVICE == "cuda":
        torch.cuda.empty_cache()
        torch.cuda.ipc_collect()
    elif DEFAULT_DEVICE == "xpu":
        torch.xpu.empty_cache()
    elif DEFAULT_DEVICE == "mps":
        torch.mps.empty_cache()


def register_hooks(hooks_registered: OrderedDict, callbacks: Union[List, Callable, Dict]):
    if callbacks is None:
        return
    if isinstance(callbacks, (Dict, OrderedDict)):
        for key, value in callbacks.items():
            hooks_registered[key] = value
        return
    if isinstance(callbacks, Callable):
        callbacks = [callbacks]
    for callback in callbacks:
        key = f"hook_{len(hooks_registered):02d}"
        while key in hooks_registered:
            key = key + "_x"
        hooks_registered[key] = callback


def standardize_module_params(params):
    if params is None:
        return
    for key, value in params.items():
        if not isinstance(value, dict) and key not in {"description"}:
            value = {"value": value}
        if isinstance(value, dict) and "data_type" not in value:
            value["data_type"] = type(value["value"])
        params[key] = value


def patch_module_params(cfg_param, module_params, module_name: str = ""):
    cfg_key_set = set(cfg_param.keys())
    module_key_set = set(module_params.keys())

    for key in cfg_key_set:
        if key not in module_key_set:
            logger.warning(f"Found invalid {module_name} config: {key}")
            cfg_param.pop(key)

    for key in module_key_set:
        if key not in cfg_key_set:
            cfg_param[key] = module_params[key]
            continue
        mparam = module_params[key]
        cparam = cfg_param[key]
        if isinstance(mparam, dict):
            target_type = mparam.get("data_type", type(mparam["value"]))
            if isinstance(cparam, dict):
                value = cparam.get("value", mparam["value"])
            else:
                value = cparam
            try:
                value = target_type(value)
            except Exception:
                value = mparam["value"]
            mparam["value"] = value
            cfg_param[key] = mparam
        else:
            try:
                cfg_param[key] = type(mparam)(cparam)
            except Exception:
                cfg_param[key] = mparam

    cfg_param["__param_patched"] = True
    return cfg_param


class BaseModule:
    params: Dict = None
    logger = logger

    _preprocess_hooks: OrderedDict = None
    _postprocess_hooks: OrderedDict = None

    download_file_list: List = None
    download_file_on_load = False

    _load_model_keys: set = None

    def __init__(self, **params) -> None:
        standardize_module_params(self.params)
        if self.params is not None and "__param_patched" not in params:
            params = patch_module_params(params, self.params, str(self))
        if params:
            if self.params is None:
                self.params = params
            else:
                self.params.update(params)

    @classmethod
    def register_postprocess_hooks(cls, callbacks: Union[List, Callable]):
        assert cls._postprocess_hooks is not None
        register_hooks(cls._postprocess_hooks, callbacks)

    @classmethod
    def register_preprocess_hooks(cls, callbacks: Union[List, Callable, Dict]):
        assert cls._preprocess_hooks is not None
        register_hooks(cls._preprocess_hooks, callbacks)

    def get_param_value(self, param_key: str):
        assert self.params is not None and param_key in self.params
        value = self.params[param_key]
        if isinstance(value, dict):
            return value["value"]
        return value

    def set_param_value(self, param_key: str, param_value, convert_dtype=True):
        assert self.params is not None and param_key in self.params
        value = self.params[param_key]
        if isinstance(value, dict):
            if convert_dtype:
                try:
                    val_type = value.get("data_type", type(value["value"]))
                    param_value = val_type(param_value)
                except Exception:
                    param_value = value["value"]
            value["value"] = param_value
        else:
            if convert_dtype:
                try:
                    param_value = type(value)(param_value)
                except Exception:
                    param_value = value
            self.params[param_key] = param_value

    def updateParam(self, param_key: str, param_content):
        self.set_param_value(param_key, param_content)

    def unload_model(self, empty_cache=False):
        model_deleted = False
        if self._load_model_keys is not None:
            for key in self._load_model_keys:
                if hasattr(self, key):
                    model = getattr(self, key)
                    if model is not None:
                        if hasattr(model, "unload_model"):
                            model.unload_model(empty_cache=False)
                        del model
                        setattr(self, key, None)
                        model_deleted = True

        if empty_cache and model_deleted:
            soft_empty_cache()
        return model_deleted

    def load_model(self):
        self._load_model()
        return

    def _load_model(self):
        return

    def all_model_loaded(self):
        if self._load_model_keys is None:
            return True
        for key in self._load_model_keys:
            if not hasattr(self, key) or getattr(self, key) is None:
                return False
        return True

    def __del__(self):
        self.unload_model()

    @property
    def debug_mode(self):
        return bool(os.environ.get("DEBUG", False))

    def flush(self, param_key: str):
        return None
