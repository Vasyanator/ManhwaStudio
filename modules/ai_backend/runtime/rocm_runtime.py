"""
File: modules/ai_backend/runtime/rocm_runtime.py

Purpose:
Configure MIOpen convolution behavior and the HIP caching allocator when the
active PyTorch build targets ROCm (AMD HIP), so Torch-backed services (LaMa,
AOT, CTD, SDXL prefill) do not pay repeated per-input-shape kernel
auto-tuning/compilation cost and never run on an allocator mode that
miscompiles on this hardware class.

Main responsibilities:
- detect a ROCm/HIP PyTorch build via `torch.version.hip`;
- switch MIOpen into immediate mode (no exhaustive Find / no per-shape JIT
  tuning) by defaulting `MIOPEN_FIND_MODE=FAST` when the user has not set it;
- force `expandable_segments:False` into the allocator-config variable Torch
  will ACTUALLY read, because that allocator mode corrupts computation on this
  ROCm stack;
- pin MIOpen's user/kernel cache to the app cache root so the small amount of
  kernel state that is still produced survives backend restarts;
- disable cuDNN/MIOpen benchmark auto-tuning explicitly;
- tell `error_text` whether this process runs on ROCm, so the user-facing
  rewrite of Torch's `expandable_segments` advice happens only where the advice
  is actually harmful.

Key functions:
- `configure_rocm_runtime()`

Notes:
- A no-op on CPU-only, CUDA, MPS, or absent-Torch installs: it only acts when
  `torch.version.hip` is a non-empty string.
- Every environment default here uses `setdefault` semantics so an explicit
  user/env override wins - with ONE deliberate exception, `expandable_segments`,
  which is overridden even when the user set it because it is a correctness
  hazard rather than a performance preference. `MS_ALLOW_EXPANDABLE_SEGMENTS=1`
  is the documented opt-out; see `_pin_expandable_segments_off`.
- WHICH variable carries the allocator config is not ours to pick. Torch reads
  `PYTORCH_CUDA_ALLOC_CONF`, then `PYTORCH_HIP_ALLOC_CONF`, then
  `PYTORCH_ALLOC_CONF`, and STOPS at the first one that is present, empty value
  included (`c10/core/AllocatorConfig.cpp`, `c10/util/env.cpp::get_env`).
  Writing our value into `PYTORCH_HIP_ALLOC_CONF` while the environment already
  carries `PYTORCH_CUDA_ALLOC_CONF` would therefore be silently inert.
- MIOpen reads `MIOPEN_FIND_MODE` at convolution Find time and the cache paths
  at handle creation (first GPU convolution), so configuring this before the
  first inference request is sufficient.
"""

from __future__ import annotations

import logging
import os
from pathlib import Path

from .error_text import configure_error_text
from .paths import program_root

log = logging.getLogger(__name__)

# MIOpen FIND_MODE=2 (FAST): use Immediate Mode and a heuristically chosen
# precompiled kernel instead of the lengthy exhaustive Find that compiles and
# benchmarks many kernel candidates for every new convolution input shape.
_MIOPEN_FIND_MODE_FAST = "2"

# The three allocator-config variables Torch consults, IN ITS OWN ORDER. It stops
# at the first one that is present in the environment, so this order is the whole
# reason `_effective_alloc_conf_var` exists - see the module docstring.
_ALLOC_CONF_VARS: tuple[str, ...] = (
    "PYTORCH_CUDA_ALLOC_CONF",
    "PYTORCH_HIP_ALLOC_CONF",
    "PYTORCH_ALLOC_CONF",
)

# Which variable to create when the environment carries none of the three. The HIP
# spelling is the honest one on a ROCm build.
_DEFAULT_ALLOC_CONF_VAR = "PYTORCH_HIP_ALLOC_CONF"

# The one allocator option this module overrides, and the exact text it writes.
_EXPANDABLE_SEGMENTS_OPTION = "expandable_segments"
_EXPANDABLE_SEGMENTS_OFF = f"{_EXPANDABLE_SEGMENTS_OPTION}:False"

# Values of `expandable_segments` that already mean "off" and are therefore left
# byte-identical. ANY other value - `True`, or something Torch itself would reject
# - is replaced, because the safe direction of an unparseable value is off.
_EXPANDABLE_SEGMENTS_OFF_TOKENS = frozenset({"false", "0", "no", "off"})

# Escape hatch: with this set, the user's allocator configuration is left exactly
# as found. It exists because silently reversing an explicit environment variable
# is its own undebuggable surprise; the reversal is still logged either way.
_ALLOW_EXPANDABLE_SEGMENTS_ENV = "MS_ALLOW_EXPANDABLE_SEGMENTS"
_TRUTHY_TOKENS = frozenset({"1", "true", "yes", "on"})


def _effective_alloc_conf_var() -> str:
    """Name the allocator-config variable Torch will ACTUALLY read in this process.

    Torch tries `PYTORCH_CUDA_ALLOC_CONF`, `PYTORCH_HIP_ALLOC_CONF`,
    `PYTORCH_ALLOC_CONF` in that order and stops at the first one that is
    PRESENT - an empty value counts as present. Returns that variable's name, or
    `PYTORCH_HIP_ALLOC_CONF` when the environment carries none of the three.
    """
    for name in _ALLOC_CONF_VARS:
        if name in os.environ:
            return name
    return _DEFAULT_ALLOC_CONF_VAR


def _pin_expandable_segments_off(raw: str | None) -> tuple[str, list[str]]:
    """Return `raw` with every `expandable_segments` option forced to `False`.

    `raw` is an allocator-config value (`key:value` items separated by commas), or
    `None`/empty when the variable does not exist yet. Returns
    `(value_to_publish, overridden)`: `overridden` lists the option texts that
    were reversed and is EMPTY when the option was absent (it is appended) or
    already off (the value comes back byte-identical). Every other option is
    preserved verbatim and in order, so a co-existing setting such as
    `max_split_size_mb:128` survives.

    All occurrences are rewritten, not just the first: Torch's parser applies
    them in order, so a later `expandable_segments:True` would otherwise win.
    Splitting on commas also splits Torch's bracketed list syntax
    (`roundup_power2_divisions:[64:1,128:2]`), which is harmless here - the parts
    are rejoined in the same order with the same separator, and no part of a
    bracket group can be mistaken for this option's key.
    """
    if raw is None or not raw.strip():
        return _EXPANDABLE_SEGMENTS_OFF, []

    items = raw.split(",")
    overridden: list[str] = []
    found = False
    for index, item in enumerate(items):
        key, separator, value = item.partition(":")
        if key.strip().lower() != _EXPANDABLE_SEGMENTS_OPTION:
            continue
        found = True
        if separator and value.strip().lower() in _EXPANDABLE_SEGMENTS_OFF_TOKENS:
            continue
        overridden.append(item.strip())
        items[index] = _EXPANDABLE_SEGMENTS_OFF
    if not found:
        return f"{raw},{_EXPANDABLE_SEGMENTS_OFF}", []
    return ",".join(items), overridden


def _expandable_segments_override_allowed() -> bool:
    """Whether the user explicitly asked to keep their own `expandable_segments` value."""
    return os.environ.get(_ALLOW_EXPANDABLE_SEGMENTS_ENV, "").strip().lower() in _TRUTHY_TOKENS


def _configure_hip_allocator() -> None:
    """Force `expandable_segments:False` into the allocator variable Torch will read.

    This is the ONE place in this module that does not use `setdefault`
    semantics: `expandable_segments:True` is a correctness hazard on this ROCm
    stack, not a performance preference, so an inherited or explicit user value
    is reversed and the reversal logged at WARNING. `MS_ALLOW_EXPANDABLE_SEGMENTS`
    opts out and leaves the environment exactly as found, also at WARNING.
    Never raises.
    """
    var_name = _effective_alloc_conf_var()
    raw = os.environ.get(var_name)

    if _expandable_segments_override_allowed():
        log.warning(
            "%s is set: leaving %s=%s exactly as found, on your explicit instruction. "
            "If expandable_segments is enabled, this ROCm build corrupts computation and "
            "the result is a fully black image at normal timings.",
            _ALLOW_EXPANDABLE_SEGMENTS_ENV,
            var_name,
            "<unset>" if raw is None else raw,
        )
        return

    pinned, overridden = _pin_expandable_segments_off(raw)
    if overridden:
        # WARNING, not info: this reverses something the user set on purpose, and
        # a reversal nobody can see is how an "it ignores my env var" bug is born.
        log.warning(
            "Overriding %s: %s -> %s. On this project's ROCm stack "
            "(torch 2.12.0+rocm7.2, HIP 7.2, gfx1201) expandable_segments:True corrupts "
            "computation: NaN above 256 px, HSA_STATUS_ERROR_ILLEGAL_INSTRUCTION at "
            "512-1536 px, and the user-visible symptom is a fully black image at normal "
            "timings. Set %s=1 to keep your own value. Every other allocator option is "
            "preserved.",
            var_name,
            ", ".join(overridden),
            _EXPANDABLE_SEGMENTS_OFF,
            _ALLOW_EXPANDABLE_SEGMENTS_ENV,
        )
    os.environ[var_name] = pinned


def _resolve_cache_root() -> Path:
    """Resolve the app cache root (`ManhwaStudio_AI_Models/.cache`).

    Falls back to the program root (`paths.program_root()`) when the ONNX
    runtime helper is unavailable, so MIOpen cache pinning still targets a
    stable directory.
    """
    try:
        from ..engines.paddle_onnx import resolve_compiled_cache_root

        return resolve_compiled_cache_root()
    except Exception:
        return program_root() / "ManhwaStudio_AI_Models" / ".cache"


def configure_rocm_runtime() -> bool:
    """Apply MIOpen immediate-mode, persistent-cache and HIP-allocator settings for ROCm Torch.

    Forces `expandable_segments:False` into the allocator-config variable Torch
    will actually read (`_configure_hip_allocator`) - the one setting here that
    overrides an explicit user value, because that allocator mode corrupts
    computation on this ROCm stack and yields black images.

    Also records for `error_text.sanitize_torch_error` whether this process runs
    on ROCm, so the user-facing rewrite of Torch's `expandable_segments` advice
    is applied only where that advice is harmful.

    Returns `True` when the active Torch build is a ROCm/HIP build and the
    settings were applied, `False` otherwise (CPU/CUDA/MPS build, Torch missing,
    or any detection failure). Never raises: configuration is best-effort and a
    failure must not break backend startup.
    """
    try:
        import torch  # type: ignore
    except Exception:
        # Torch absent (ONNX-only install): nothing MIOpen-related to configure.
        configure_error_text(rocm_runtime=False)
        return False

    hip_version = getattr(getattr(torch, "version", None), "hip", None)
    if not isinstance(hip_version, str) or not hip_version.strip():
        # CUDA / CPU / MPS Torch build: MIOpen is not used, and Torch's own
        # `expandable_segments` advice is CORRECT there - do not rewrite it.
        configure_error_text(rocm_runtime=False)
        return False

    configure_error_text(rocm_runtime=True)

    # Immediate mode: skip per-shape exhaustive Find/compile. setdefault keeps an
    # explicit user override intact.
    os.environ.setdefault("MIOPEN_FIND_MODE", _MIOPEN_FIND_MODE_FAST)

    # NOT a setdefault, and deliberately so. On the ROCm stack this project
    # targets (torch 2.12.0+rocm7.2, HIP 7.2, gfx1201) `expandable_segments:True`
    # CORRUPTS computation: VAE decode above 256 px yields NaN in a different
    # module on every run (GroupNorm / SiLU / Conv2d / Linear - memory
    # corruption, not arithmetic), and at 512-1536 px the process takes
    # HSA_STATUS_ERROR_ILLEGAL_INSTRUCTION and wedges the HSA queue. The
    # user-visible symptom is a fully black image with normal timings, i.e. a
    # silent wrong result, so honouring an inherited value would hand the user a
    # broken run rather than an error.
    # Timing is sufficient here: Torch's allocator config is a lazily initialized
    # singleton that reads the variable at the FIRST GPU allocation, not at
    # `import torch`, and this function runs in `ai_backend.py::main()` before
    # `run_server()` and before anything touches the GPU.
    _configure_hip_allocator()

    # Pin MIOpen user perf-db and compiled-kernel cache to the app cache root so
    # any kernel state that is still produced is reused across backend restarts.
    cache_root = _resolve_cache_root() / "miopen"
    user_db = cache_root / "user_db"
    kernel_cache = cache_root / "kernels"
    try:
        user_db.mkdir(parents=True, exist_ok=True)
        kernel_cache.mkdir(parents=True, exist_ok=True)
        os.environ.setdefault("MIOPEN_USER_DB_PATH", str(user_db))
        os.environ.setdefault("MIOPEN_CUSTOM_CACHE_DIR", str(kernel_cache))
    except OSError as exc:
        # Cache pinning is an optimization; fall back to MIOpen defaults on a
        # filesystem error instead of failing backend startup.
        log.warning(
            "MIOpen cache directory could not be prepared at %s: %s. "
            "Using MIOpen default cache location.",
            cache_root,
            exc,
        )

    # Disable cuDNN/MIOpen benchmark auto-tuning explicitly (defensive: another
    # module could have enabled it). benchmark=True re-runs Find per new shape.
    try:
        torch.backends.cudnn.benchmark = False
    except Exception as exc:
        log.warning("Could not disable cudnn/MIOpen benchmark: %s", exc)

    # Report the variable Torch will really consult, not a fixed name: which of
    # the three it is depends on the inherited environment.
    alloc_conf_var = _effective_alloc_conf_var()
    log.info(
        "ROCm Torch build detected (hip=%s); MIOpen immediate mode enabled "
        "(MIOPEN_FIND_MODE=%s), benchmark disabled, cache pinned to %s, %s=%s.",
        hip_version.strip(),
        os.environ.get("MIOPEN_FIND_MODE"),
        cache_root,
        alloc_conf_var,
        os.environ.get(alloc_conf_var),
    )
    return True
