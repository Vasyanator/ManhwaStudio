"""
Package: modules/ai_backend/inpaint/flux3_klein

Purpose:
FLUX.3 Image Klein (BFL, 5.6B distilled flow-map DiT) for the Python AI
backend: caption + up to ten reference images -> image, from the official
weight package (bf16, fp8r or nvfp4 DiT, or a GGUF DiT file; float32 AE;
Qwen3-VL text encoder, or a GGUF file of its text half),
through `Flux3KleinService`, driven by the large-model manager
(`runtime.large_models`) over two handles, the pipeline (DiT + AE) and the
text encoder (their specs are `large_model.py`).

Integration boundary: `server.py` constructs the service (`AppState.flux3_klein`);
the IPC methods `inpaint.flux3_klein*` (`ipc/handlers/flux3_klein.py`) run the
cleaning tab's region editor through `region.py`, whose params are mapped by
`wire.py`, and the prompt library (`.prompt_cache.*`); the calibration tool `tools/large_model_calibration/` drives the
service directly.

Module layout (dependency direction strictly downwards; see `MODULE_README.md`):

- `config.py`       - package constants, `config.json`, weight-format detection, DiT / encoder sources, files.
- `params.py`       - option enums, `Flux3Options`, `Flux3Request`, `normalize_request`.
- `phases.py`       - `Phase`, `PhaseObserver`, `observed`.
- `quant.py`        - fp8r / nvfp4 linears, the `_scaled_mm` probe.
- `packing.py`      - references, image <-> tensor, `Packed`, `pack`, noise.
- `dit.py`          - `Flux3Dit`.
- `autoencoder.py`  - the AE and its streaming loader.
- `sampling.py`     - the flow-map schedule and Euler loop.
- `loading.py`      - `load_dit` (header checks, quantized swap, streaming fill).
- `text_encoder.py` - the truncated Qwen3-VL encoder, TEXT and VL_REFS encodes.
- `large_model.py`  - the pipeline / text-encoder `LargeModelSpec`s, residency keys, formats, `FLUX3_PAIR`, request shape.
- `prompt_cache.py` - the prompt library side (TEXT mode): library id, encoder identity, the cache adapter.
- `wire.py`         - the IPC params -> `Flux3Options` mapping, the region frame rule.
- `service.py`      - `Flux3KleinService` (runs, status, the prompt cache), `Flux3Canceled`.
- `region.py`       - the region editor (raw answer, mask validation, seed, progress stages, cancel) and the prompt-cache build's frames.

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
    DitSource,
    EncoderSource,
    GgufDit,
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
    "DitSource",
    "EncoderSource",
    "GgufDit",
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
