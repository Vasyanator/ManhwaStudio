"""
Package: modules/ai_backend/inpaint/flux3_klein

Purpose:
FLUX.3 Image Klein (BFL, 5.6B distilled flow-map DiT) for the Python AI
backend: caption + up to ten reference images -> image, from the official
weight package (bf16, fp8r or nvfp4 DiT; float32 AE; Qwen3-VL text encoder),
through `Flux3KleinService`, driven by the large-model manager
(`runtime.large_models`; the model's spec is `large_model.py`).

Integration boundary: the service is NOT constructed in `server.py`, has no
`AppState` field, no IPC method and no Rust side, and `PROTOCOL_VERSION` is
unchanged. Its one caller is the VRAM / parity script in `test/`.

Module layout (dependency direction strictly downwards; see `MODULE_README.md`):

- `config.py`       - package constants, `config.json`, weight-format detection, files.
- `params.py`       - option enums, `Flux3Options`, `Flux3Request`, `normalize_request`.
- `phases.py`       - `Phase`, `PhaseObserver`, `observed`.
- `quant.py`        - fp8r / nvfp4 linears, the `_scaled_mm` probe.
- `packing.py`      - references, image <-> tensor, `Packed`, `pack`, noise.
- `dit.py`          - `Flux3Dit`.
- `autoencoder.py`  - the AE and its streaming loader.
- `sampling.py`     - the flow-map schedule and Euler loop.
- `loading.py`      - `load_dit` (header checks, quantized swap, streaming fill).
- `text_encoder.py` - the truncated Qwen3-VL encoder, TEXT and VL_REFS encodes.
- `large_model.py`  - the `LargeModelSpec`s, residency key, formats, request shape.
- `service.py`      - `Flux3KleinService`.

Notes:
- Importing the package does NOT import torch: `config`, `params`, `phases`, `large_model` and
  `service` are torch-free at import time and re-exported eagerly; the names of
  the torch-side modules are resolved on first access (PEP 562 `__getattr__`,
  the same mechanism `modules/ai_backend/__init__.py` uses for `run_server`).
- Patch points (`service._resolve_selected_backend_device`,
  `service._clear_torch_cache`, the loaders and encoders of the torch-side
  modules) are patched on their DEFINING module, never on this package: a
  re-export is a separate binding.
"""

from __future__ import annotations

import importlib
import logging
from typing import Any

from .config import (  # noqa: F401  - re-exported public surface
    MAX_REFERENCES,
    ModelFiles,
    PackageConfig,
    WeightFormat,
    read_package_config,
    resolve_model_files,
    variant_availability,
)
from .params import (  # noqa: F401  - re-exported public surface
    AeDtype,
    EncoderMode,
    Flux3Options,
    Flux3Request,
    Fp8Compute,
)
from .phases import Phase, PhaseObserver  # noqa: F401  - re-exported public surface
from .service import Flux3Applied, Flux3KleinService, Flux3Result  # noqa: F401  - re-exported public surface

#: Package-level logger: the common parent of every submodule's logger.
log = logging.getLogger(__name__)

#: Re-exported names of the torch-side modules -> their defining submodule,
#: imported on first access so that importing the package stays torch-free.
_LAZY_EXPORTS: dict[str, str] = {
    "load_dit": "loading",
    "LoadedDit": "loading",
    "load_autoencoder": "autoencoder",
    "load_text_encoder": "text_encoder",
    "encode_text": "text_encoder",
    "encode_with_references": "text_encoder",
    "pack": "packing",
    "initial_noise": "packing",
    "flow_map_schedule": "sampling",
    "set_fp8_compute": "quant",
    "probe_scaled_mm": "quant",
}

__all__ = [
    "MAX_REFERENCES",
    "AeDtype",
    "EncoderMode",
    "Flux3Applied",
    "Flux3KleinService",
    "Flux3Options",
    "Flux3Request",
    "Flux3Result",
    "Fp8Compute",
    "ModelFiles",
    "PackageConfig",
    "Phase",
    "PhaseObserver",
    "WeightFormat",
    "read_package_config",
    "resolve_model_files",
    "variant_availability",
    *_LAZY_EXPORTS,
]


def __getattr__(name: str) -> Any:
    """Resolve a torch-side re-export on first access (PEP 562).

    # Raises
    `AttributeError` for a name this package does not export.
    """
    module_name = _LAZY_EXPORTS.get(name)
    if module_name is None:
        raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
    value = getattr(importlib.import_module(f".{module_name}", __name__), name)
    # Cache the binding so later lookups skip this hook.
    globals()[name] = value
    return value


def __dir__() -> list[str]:
    """The eager names plus the lazy re-exports."""
    return sorted({*globals(), *_LAZY_EXPORTS})
