"""
File: modules/ai_backend/inpaint/flux3_klein/loading.py

Purpose:
Load a FLUX.3 klein DiT checkpoint (bf16, fp8r or nvfp4 export) into a
`Flux3Dit` on a device, streaming one tensor at a time.

Main responsibilities:
- refuse a wrong checkpoint from its header before any tensor byte is read
  (`require_flow_map`, format detection vs `config.json` vs the requested
  variant);
- build the model on `meta`, swap in the quantized linears the header calls for,
  fill it through `runtime.safetensors_stream.fill_module_from_safetensors`;
- resolve and apply the fp8 compute path;
- the per-tensor conversion the large-model spec loads the DiT with
  (`convert_dit_tensor`, a key/dtype rule equal to `dit_tensor_cast` once
  `build_dit_meta` accepted the header).

Key declarations:
- `build_dit_meta()` (header checks + meta model, shared by both loaders)
- `load_dit()` -> `LoadedDit` (the standalone loader: parity script, tests)
- `require_flow_map()`, `dit_tensor_cast()`, `convert_dit_tensor()`

Notes:
- The cast rule follows black-forest-labs/flux-3-image-klein
  `src/flux3/checkpoint.py` (commit e53f4ea, Apache-2.0, see `LICENSE` in
  this directory): quantization payloads (weights and scales of the quantized
  layers) load as stored, other floating tensors become bf16, non-floating ones
  are kept. Modified: streaming per-tensor fill instead of `load_file` of the
  whole checkpoint; a quantized payload whose stored dtype differs from the
  layer's buffer is refused instead of loaded; NVFP4 loads on any device
  (dequant path, no FlashInfer swizzle).
- The fp8 path is resolved (probe) BEFORE the fill, so an explicit
  `SCALED_MM` that cannot run fails before gigabytes are read (the service
  resolves it before the lease and hands the concrete path to
  `build_dit_meta`).
- Where the DiT's bytes live during a run (device, RAM, disk, block by block)
  is the large-model manager's decision; nothing here moves a loaded DiT.
"""

from __future__ import annotations

import logging
import time
from collections.abc import Callable, Iterator, Mapping
from dataclasses import dataclass
from typing import Any

import torch
from torch import Tensor, nn

from ...runtime.safetensors_stream import (
    SAFETENSORS_DTYPES_WITH_F8,
    ByteProgressCb,
    fill_module_from_safetensors,
    read_safetensors_header,
)
from .config import (
    FLOW_MAP_KEY,
    ModelFiles,
    PackageConfig,
    WeightFormat,
    detect_weight_format,
    require_declared_format,
)
from .dit import Flux3Dit
from .params import Fp8Compute
from .quant import QUANT_SUFFIXES, Fp8RowwiseLinear, Nvfp4Linear, resolve_fp8_compute, set_fp8_compute, swap_quantized_linears

log = logging.getLogger(__name__)

#: Owner name in the shared loader's messages and log records.
_OWNER = "FLUX.3 klein DiT"


@dataclass
class LoadedDit:
    """A loaded DiT in eval mode on its device.

    `format` is the checkpoint's weight format; `fp8_compute_applied` is the
    concrete fp8 path set on its `Fp8RowwiseLinear`s (`None` for a bf16 or nvfp4
    checkpoint, which has none); `bytes_read` counts the tensor bytes read.
    """

    dit: Flux3Dit
    format: WeightFormat
    fp8_compute_applied: Fp8Compute | None
    bytes_read: int


def require_flow_map(header: Mapping[str, Any]) -> None:
    """Refuse a checkpoint without `FLOW_MAP_KEY`: it is the CFG "base" model.

    # Raises
    `ValueError` saying the base/CFG checkpoint is out of scope.
    """
    if FLOW_MAP_KEY not in header:
        raise ValueError(
            f"Чекпоинт DiT FLUX.3 klein не содержит «{FLOW_MAP_KEY}»: это базовая модель с CFG, "
            "она не поддерживается (нужна дистиллированная модель flow map)."
        )


def _quantized_buffer_dtypes(module: nn.Module) -> dict[str, torch.dtype]:
    """`state_dict` key -> buffer dtype for every buffer of a quantized linear in `module`."""
    result: dict[str, torch.dtype] = {}
    for name, child in module.named_modules():
        if isinstance(child, (Fp8RowwiseLinear, Nvfp4Linear)):
            for buffer_name, buffer in child.named_buffers(recurse=False):
                result[f"{name}.{buffer_name}"] = buffer.dtype
    return result


def dit_tensor_cast(quantized_dtypes: Mapping[str, torch.dtype]) -> Callable[[str, Tensor], Tensor]:
    """The per-tensor cast rule for `fill_module_from_safetensors`.

    `quantized_dtypes` maps each quantized-layer buffer key to its dtype
    (`_quantized_buffer_dtypes`). Such a tensor must already carry that dtype
    and is returned as stored; any other floating tensor becomes bf16; a
    non-floating one is kept.

    The returned callable raises `ValueError` for a quantized payload stored in
    another dtype, or for an 8-bit tensor outside a quantized layer.
    """

    def cast(key: str, tensor: Tensor) -> Tensor:
        """`tensor` as stored for a quantized buffer `key` (refused when mis-typed), else the rule above."""
        expected = quantized_dtypes.get(key)
        if expected is not None:
            if tensor.dtype != expected:
                raise ValueError(
                    f"{_OWNER}: тензор «{key}» хранится как {tensor.dtype}, а квантованный слой ждёт {expected}"
                )
            return tensor
        if tensor.dtype in (torch.float8_e4m3fn, torch.float8_e5m2, torch.uint8):
            raise ValueError(f"{_OWNER}: 8-битный тензор «{key}» ({tensor.dtype}) вне квантованного слоя")
        return tensor.to(torch.bfloat16) if tensor.is_floating_point() else tensor

    return cast


#: Header dtype tokens of 8-bit payloads (quantized weights and NVFP4 block scales).
_EIGHT_BIT_TOKENS = frozenset({"F8_E4M3", "F8_E5M2", "U8"})

#: Leaf names of the quantized layers' scale buffers (`quant.QUANT_SUFFIXES` without the dot).
_QUANT_LEAVES = frozenset(suffix.lstrip(".") for suffix in QUANT_SUFFIXES)


def _require_quantized_dtypes(dit: nn.Module, header: Mapping[str, Any], source: Any) -> None:
    """Refuse a header whose quantized payload dtypes do not match the swapped layers, or an 8-bit tensor outside them.

    The header-level form of `dit_tensor_cast`'s refusals, so a loader that
    converts by rule (`convert_dit_tensor`) refuses the same checkpoints
    before any tensor byte is read.

    # Raises
    `ValueError` naming the first offending key.
    """
    expected = _quantized_buffer_dtypes(dit)
    for key, entry in header.items():
        if not isinstance(entry, Mapping) or "dtype" not in entry:
            continue
        token = str(entry["dtype"])
        dtype_name = SAFETENSORS_DTYPES_WITH_F8.get(token, (None, 0))[0]
        wanted = expected.get(key)
        if wanted is not None:
            if dtype_name is None or getattr(torch, dtype_name) != wanted:
                raise ValueError(f"{_OWNER}: тензор «{key}» хранится как {token}, а квантованный слой ждёт {wanted} ({source})")
        elif token in _EIGHT_BIT_TOKENS:
            raise ValueError(f"{_OWNER}: 8-битный тензор «{key}» ({token}) вне квантованного слоя ({source})")


def build_dit_meta(files: ModelFiles, config: PackageConfig, *, variant: WeightFormat, fp8_compute_applied: Fp8Compute | None) -> Flux3Dit:
    """The `Flux3Dit` of `files.dit_file` on `meta`, quantized linears swapped, checked against its header.

    Steps: read the header; `require_flow_map`; detect the format and check it
    against `config.json` (`require_declared_format`) and against `variant`;
    build on `meta`, swap the quantized linears, check the quantized payload
    dtypes (`_require_quantized_dtypes`); for fp8r set `fp8_compute_applied`
    (a CONCRETE path, resolved by the caller) on every fp8 layer. Header reads
    only.

    # Raises
    `ValueError` for an unreadable/malformed header, a CFG checkpoint, an
    unrecognized format, a format other than `config.json` declares or
    `variant` asks for, mismatched quantized dtypes, or an fp8r checkpoint
    without a concrete `fp8_compute_applied`.
    """
    path = files.dit_file
    header = read_safetensors_header(path)
    require_flow_map(header)
    detected = detect_weight_format(header, source=path)
    require_declared_format(config, detected)
    if detected is not variant:
        raise ValueError(f"Запрошен вариант DiT «{variant.value}», а {path} содержит «{detected.value}».")
    with torch.device("meta"):
        dit = Flux3Dit(config.dit)
    swapped = swap_quantized_linears(dit, header.keys())
    if swapped is not detected:
        raise ValueError(f"{path}: ключи масштабов указывают на формат «{swapped.value}», а заголовок — на «{detected.value}».")
    _require_quantized_dtypes(dit, header, path)
    if detected is WeightFormat.FP8R:
        if fp8_compute_applied is None or fp8_compute_applied is Fp8Compute.AUTO:
            raise ValueError(f"{_OWNER}: для fp8r нужен выбранный путь вычисления (scaled_mm или dequant), получено {fp8_compute_applied}")
        set_fp8_compute(dit, fp8_compute_applied)
    return dit


def convert_dit_tensor(key: str, tensor: Tensor) -> Iterator[tuple[str, Tensor]]:
    """The DiT conversion of the large-model spec: `(key, tensor)` with the cast rule of `dit_tensor_cast`.

    Quantized payloads (an 8-bit tensor, or a scale buffer `weight_scale` /
    `weight_scale_2` / `input_scale`) are yielded as stored, other floating
    tensors as bf16, non-floating ones as stored. Device-agnostic (runs on
    `meta` to derive the stored layout). Equal to `dit_tensor_cast` for every
    header `build_dit_meta` accepts (it refuses mis-typed payloads up front).
    """
    leaf = key.rpartition(".")[2]
    if leaf in _QUANT_LEAVES or tensor.dtype in (torch.float8_e4m3fn, torch.float8_e5m2, torch.uint8):
        yield key, tensor
    elif tensor.is_floating_point():
        yield key, tensor.to(torch.bfloat16)
    else:
        yield key, tensor


def load_dit(
    files: ModelFiles,
    config: PackageConfig,
    *,
    variant: WeightFormat,
    device: Any,
    fp8_compute: Fp8Compute,
    progress: ByteProgressCb | None = None,
) -> LoadedDit:
    """Load `files.dit_file` as a `Flux3Dit` of `config.dit` onto `device`.

    Steps: read the header; `require_flow_map`; detect the format and check it
    against `config.json` (`require_declared_format`) and against `variant`;
    for fp8r resolve `fp8_compute` (probe); build on `meta`, swap the quantized
    linears, fill one tensor at a time (`progress` gets byte progress); `.eval()`
    and apply the fp8 path.

    # Raises
    `ValueError` for an unreadable/malformed header, a CFG checkpoint, an
    unrecognized format, a format other than `config.json` declares or
    `variant` asks for, a missing / unexpected / mis-shaped / mis-typed tensor
    (see `fill_module_from_safetensors`, `dit_tensor_cast`); `RuntimeError` when
    `SCALED_MM` is requested and does not run on `device`. On failure no model
    is returned.
    """
    started = time.monotonic()
    path = files.dit_file
    # The fp8 path is resolved (probe) before the meta build and the fill.
    header = read_safetensors_header(path)
    require_flow_map(header)
    detected = detect_weight_format(header, source=path)
    applied = resolve_fp8_compute(fp8_compute, device) if detected is WeightFormat.FP8R and detected is variant else None
    dit = build_dit_meta(files, config, variant=variant, fp8_compute_applied=applied)
    report = fill_module_from_safetensors(
        dit,
        [path],
        device=device,
        key_map=lambda key: key,
        cast=dit_tensor_cast(_quantized_buffer_dtypes(dit)),
        dtypes=SAFETENSORS_DTYPES_WITH_F8,
        progress=progress,
        owner=_OWNER,
        logger=log,
    )
    dit.eval()
    log.info(
        "FLUX.3 klein: DiT %s загружен на %s (fp8: %s, %.2f ГиБ, %.1f с)",
        detected.value,
        device,
        applied.value if applied is not None else "-",
        report.bytes_read / 1024**3,
        time.monotonic() - started,
    )
    return LoadedDit(dit=dit, format=detected, fp8_compute_applied=applied, bytes_read=report.bytes_read)
