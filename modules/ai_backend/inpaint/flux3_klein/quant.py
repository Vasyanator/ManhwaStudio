"""
File: modules/ai_backend/inpaint/flux3_klein/quant.py

Purpose:
The quantized linear layers of the FLUX.3 klein DiT exports (fp8r: rowwise
FP8; nvfp4: packed E2M1 with E4M3 block scales), their dequantization, the
`torch._scaled_mm` capability probe, and the swap of `nn.Linear`s into them.

Main responsibilities:
- `Fp8RowwiseLinear`: E4M3 weight `[N, K]` + f32 row scale `[N]`; forward by
  rowwise `torch._scaled_mm` (`SCALED_MM`) or by dequantizing to bf16
  (`DEQUANT`), chosen per layer through its `compute` attribute;
- `Nvfp4Linear`: packed weight `[N, K/2]` + E4M3 block scales `[N, K/16]` +
  f32 global scale; forward always dequantizes to bf16 (no FP4 kernel exists
  for gfx1201);
- `dequantize_fp8_rows`, `dequantize_nvfp4`, `quantize_rows`;
- `probe_scaled_mm` / `ScaledMmProbe`, `resolve_fp8_compute`, `set_fp8_compute`;
- `swap_quantized_linears` (header keys -> `WeightFormat`).

Notes:
- Ported from black-forest-labs/flux-3-image-klein `src/flux3/fp8.py`,
  `src/flux3/nvfp4.py` and `src/flux3/checkpoint.py` (`swap_linears`)
  (commit e53f4ea), Copyright 2026 Black Forest Labs, Inc., licensed under the
  Apache License, Version 2.0 (see `LICENSE` in this directory).
- Modified:
  - `FP8Linear` renamed `Fp8RowwiseLinear`; a `DEQUANT` path (bf16 weight per
    forward) added beside the reference `_scaled_mm` path, selected per layer
    and never implicitly (`AUTO` at forward is an error);
  - FlashInfer NVFP4 (`nvfp4_quantize` + `mm_fp4`, Blackwell only) replaced by
    a per-forward dequantization to bf16 following the export manifest's
    `scale_convention`; the block scales stay row-major (no swizzle) and
    `input_scale` is loaded but unused on this path;
  - `swap_linears` returns a `WeightFormat`, refuses a biased linear, and is
    split from the loader;
  - the `_scaled_mm` capability probe and the AUTO/SCALED_MM/DEQUANT
    resolution are new.
- The weights stay QUANTIZED in device memory; a dequantizing forward holds one
  transient bf16 weight (one layer) plus row-chunk temporaries
  (`_CHUNK_ELEMENTS`), never the whole model in bf16.
"""

from __future__ import annotations

import logging
import math
import threading
from collections.abc import Collection
from dataclasses import dataclass
from typing import Any

import torch
import torch.nn.functional as F
from torch import Tensor, nn

from .config import WeightFormat
from .params import Fp8Compute

log = logging.getLogger(__name__)

#: Storage dtypes of the exports.
FP8 = torch.float8_e4m3fn
NVFP4_PACKED = torch.uint8

#: Largest finite E4M3 magnitude (activation quantization range).
FP8_MAX = 448.0

#: Key suffixes that mark a quantized linear in a header. NVFP4 is checked first:
#: its block scales are ALSO named `.weight_scale`.
FP8_SCALE_SUFFIX = ".weight_scale"
NVFP4_SCALE_SUFFIX = ".weight_scale_2"
QUANT_SUFFIXES: tuple[str, ...] = (".weight_scale", ".weight_scale_2", ".input_scale")

#: `_scaled_mm` alignment of the reference: N and K padded to 256, rows to 128.
GEMM_ALIGNMENT = 256
ROW_ALIGNMENT = 128

#: NVFP4 elements per E4M3 block scale.
NVFP4_BLOCK = 16

#: Signed E2M1 code -> value: bit 3 is the sign, bits 0-2 index the magnitudes
#: `[0, .5, 1, 1.5, 2, 3, 4, 6]` (manifest `scale_convention`).
_E2M1_VALUES: tuple[float, ...] = (0.0, 0.5, 1.0, 1.5, 2.0, 3.0, 4.0, 6.0)

#: Elements per dequantization chunk: bounds the temporaries of one layer's
#: dequantization (4 Mi elements = 16 MiB per float32 temporary, 32 MiB for the
#: int64 LUT index `codes.long()`, 4 MiB per uint8 code tensor).
_CHUNK_ELEMENTS = 1 << 22

#: Probe GEMM shape `(rows, K, N)` and the relative-error ceiling against the
#: dequantized result (the activation quantization alone measured ~2.6% on gfx1201).
_PROBE_SHAPE = (128, 256, 256)
_PROBE_MAX_REL_ERROR = 0.05


def _ceil(value: int, alignment: int) -> int:
    """`value` rounded up to a multiple of `alignment`."""
    return (value + alignment - 1) // alignment * alignment


def _chunk_rows(columns: int) -> int:
    """Rows per dequantization chunk for a weight of `columns` elements per row (at least 1)."""
    return max(1, _CHUNK_ELEMENTS // max(1, columns))


def quantize_rows(value: Tensor) -> tuple[Tensor, Tensor]:
    """Rowwise E4M3 quantization over the last dimension (reference `fp8.quantize_rows`).

    Returns `(quantized shaped like value, f32 scale [rows])` with
    `value ~= quantized * scale[:, None]`; an all-zero row gets scale 1e-12.
    """
    rows = value.reshape(-1, value.shape[-1]).float()
    scale = (rows.abs().amax(dim=1) / FP8_MAX).clamp(min=1e-12)
    quantized = (rows / scale[:, None]).clamp(-FP8_MAX, FP8_MAX).to(FP8)
    return quantized.reshape_as(value), scale


def dequantize_fp8_rows(weight: Tensor, scale: Tensor, dtype: torch.dtype = torch.bfloat16) -> Tensor:
    """`weight.float() * scale[:, None]` as `dtype` (the fp8r manifest convention).

    `weight` is E4M3 `[N, K]`, `scale` f32 `[N]`; the result is a new `[N, K]`
    tensor on `weight`'s device, computed in row chunks so the float32
    temporaries stay small.

    # Raises
    `ValueError` when the shapes do not form a rowwise pair.
    """
    if weight.dim() != 2 or scale.shape != (weight.shape[0],):
        raise ValueError(
            f"fp8r: форма веса {tuple(weight.shape)} и масштаба {tuple(scale.shape)} не согласованы "
            "(ожидается [N, K] и [N])"
        )
    rows, columns = weight.shape
    out = torch.empty(rows, columns, dtype=dtype, device=weight.device)
    step = _chunk_rows(columns)
    for start in range(0, rows, step):
        end = min(rows, start + step)
        out[start:end] = (weight[start:end].float() * scale[start:end].float()[:, None]).to(dtype)
    return out


def dequantize_nvfp4(
    packed: Tensor, block_scale: Tensor, global_scale: Tensor, *, dtype: torch.dtype = torch.bfloat16
) -> Tensor:
    """Unpack an NVFP4 weight to `dtype` `[N, K]`.

    `packed` is uint8 `[N, K/2]` (two E2M1 codes per byte, the EVEN element in
    the LOW nibble), `block_scale` E4M3 `[N, K/16]` row-major, `global_scale` a
    one-element f32 tensor. Element `[n, k]` is
    `e2m1 * block_scale[n, k // 16] * global_scale`, multiplied in float32 and
    rounded to `dtype` once. Computed in row chunks on `packed`'s device.

    # Raises
    `ValueError` for a non-uint8 / non-2-D `packed`, a block-scale shape other
    than `[N, K/16]`, `K` not a multiple of 16, or a global scale that is not
    one element.
    """
    if packed.dtype != NVFP4_PACKED or packed.dim() != 2:
        raise ValueError(f"nvfp4: упакованный вес должен быть uint8 [N, K/2], а не {packed.dtype} {tuple(packed.shape)}")
    rows, half = packed.shape
    columns = 2 * half
    if columns % NVFP4_BLOCK or block_scale.shape != (rows, columns // NVFP4_BLOCK):
        raise ValueError(
            f"nvfp4: форма блочных масштабов {tuple(block_scale.shape)} не подходит к весу "
            f"{rows}x{columns} (ожидается [{rows}, {columns // NVFP4_BLOCK}], K кратно {NVFP4_BLOCK})"
        )
    if global_scale.numel() != 1:
        raise ValueError(f"nvfp4: глобальный масштаб должен быть скаляром, а не {tuple(global_scale.shape)}")
    device = packed.device
    magnitudes = torch.tensor(_E2M1_VALUES, dtype=torch.float32, device=device)
    lut = torch.cat((magnitudes, -magnitudes))
    global_value = global_scale.float().reshape(())
    out = torch.empty(rows, columns, dtype=dtype, device=device)
    step = _chunk_rows(columns)
    for start in range(0, rows, step):
        end = min(rows, start + step)
        chunk = packed[start:end]
        # Interleave (low, high) so byte j yields elements 2j (low) and 2j+1 (high).
        codes = torch.stack((chunk & 0x0F, chunk >> 4), dim=-1).reshape(end - start, columns)
        values = lut[codes.long()].reshape(end - start, columns // NVFP4_BLOCK, NVFP4_BLOCK)
        values = values * block_scale[start:end].float()[..., None]
        out[start:end] = (values.reshape(end - start, columns) * global_value).to(dtype)
    return out


class Fp8RowwiseLinear(nn.Module):
    """Bias-free linear over an fp8r weight: buffers `weight` (E4M3 `[N, K]`)
    and `weight_scale` (f32 `[N]`).

    `compute` selects the matmul: `SCALED_MM` quantizes the activations per row
    and calls rowwise `torch._scaled_mm` (the reference path); `DEQUANT` runs
    `F.linear` on a bf16 copy of the weight made per forward. The output is
    always bf16. `compute` starts as `AUTO`, which a forward refuses: the loader
    (or `set_fp8_compute`) must choose explicitly.
    """

    def __init__(self, in_features: int, out_features: int, device: Any = None) -> None:
        super().__init__()
        self.in_features, self.out_features = in_features, out_features
        self.compute = Fp8Compute.AUTO
        self.register_buffer("weight", torch.empty(out_features, in_features, dtype=FP8, device=device))
        self.register_buffer("weight_scale", torch.empty(out_features, dtype=torch.float32, device=device))

    def extra_repr(self) -> str:
        return f"in_features={self.in_features}, out_features={self.out_features}, compute={self.compute.value}"

    def forward(self, x: Tensor) -> Tensor:
        """`x [..., K]` -> bf16 `[..., N]`.

        # Raises
        `RuntimeError` when `compute` is still `AUTO`.
        """
        leading = tuple(x.shape[:-1])
        rows = math.prod(leading)
        if rows == 0:
            return x.new_zeros(*leading, self.out_features, dtype=torch.bfloat16)
        if self.compute is Fp8Compute.DEQUANT:
            return F.linear(x.to(torch.bfloat16), dequantize_fp8_rows(self.weight, self.weight_scale))
        if self.compute is Fp8Compute.SCALED_MM:
            return self._scaled_mm_forward(x, leading, rows)
        raise RuntimeError(
            "Fp8RowwiseLinear: режим вычисления fp8 не выбран (AUTO); вызовите set_fp8_compute "
            "с SCALED_MM или DEQUANT после загрузки"
        )

    def _scaled_mm_forward(self, x: Tensor, leading: tuple[int, ...], rows: int) -> Tensor:
        """The reference `FP8Linear.forward`: pad to the GEMM alignment, quantize rows, `_scaled_mm`."""
        flat = x.to(torch.bfloat16).reshape(rows, self.in_features)
        n, k, m = _ceil(self.out_features, GEMM_ALIGNMENT), _ceil(self.in_features, GEMM_ALIGNMENT), _ceil(rows, ROW_ALIGNMENT)
        weight, scale = self.weight, self.weight_scale
        if (n, k) != (self.out_features, self.in_features):
            # Pad through a uint8 view: F.pad has no float8 kernels.
            weight = F.pad(weight.view(torch.uint8), (0, k - self.in_features, 0, n - self.out_features)).view(FP8)
            scale = F.pad(scale, (0, n - self.out_features), value=1.0)
        if (m, k) != (rows, self.in_features):
            flat = F.pad(flat, (0, k - self.in_features, 0, m - rows))
        q, q_scale = quantize_rows(flat.contiguous())
        out = torch._scaled_mm(
            q,
            weight.T,
            q_scale[:, None].contiguous(),
            scale[None, :].contiguous(),
            out_dtype=torch.bfloat16,
            use_fast_accum=True,
        )
        return out[:rows, : self.out_features].reshape(*leading, self.out_features)


class Nvfp4Linear(nn.Module):
    """Bias-free linear over an NVFP4 weight (ModelOpt layout, row-major scales).

    Buffers: `weight` uint8 `[N, K/2]`, `weight_scale` E4M3 `[N, K/16]`,
    `weight_scale_2` f32 `[]` (global), `input_scale` f32 `[]` (the export's
    static activation scale: loaded so the key set matches, unused here because
    the activations are not quantized on the dequant path). The forward is
    `F.linear(x.bf16, dequantize_nvfp4(...))`; the output is bf16.
    """

    def __init__(self, in_features: int, out_features: int, device: Any = None) -> None:
        super().__init__()
        if in_features % NVFP4_BLOCK:
            raise ValueError(f"Nvfp4Linear: in_features={in_features} не кратно {NVFP4_BLOCK}")
        self.in_features, self.out_features = in_features, out_features
        self.register_buffer("weight", torch.empty(out_features, in_features // 2, dtype=NVFP4_PACKED, device=device))
        self.register_buffer(
            "weight_scale", torch.empty(out_features, in_features // NVFP4_BLOCK, dtype=FP8, device=device)
        )
        self.register_buffer("weight_scale_2", torch.empty((), dtype=torch.float32, device=device))
        self.register_buffer("input_scale", torch.empty((), dtype=torch.float32, device=device))

    def extra_repr(self) -> str:
        return f"in_features={self.in_features}, out_features={self.out_features}"

    def forward(self, x: Tensor) -> Tensor:
        """`x [..., K]` -> bf16 `[..., N]` through a transient bf16 weight."""
        leading = tuple(x.shape[:-1])
        if math.prod(leading) == 0:
            return x.new_zeros(*leading, self.out_features, dtype=torch.bfloat16)
        weight = dequantize_nvfp4(self.weight, self.weight_scale, self.weight_scale_2)
        return F.linear(x.to(torch.bfloat16), weight)


@dataclass(frozen=True)
class ScaledMmProbe:
    """Outcome of `probe_scaled_mm`: whether rowwise `_scaled_mm` runs and agrees
    with the dequantized result on this device, why not if it does not, the
    measured relative L2 error (`None` when it did not run), and `transient`:
    the run failed for lack of memory, so the answer says nothing about the
    capability and was not cached."""

    supported: bool
    reason: str
    rel_error: float | None
    transient: bool = False


#: Definitive probe answers per `str(device)` (see `probe_scaled_mm`), guarded
#: by `_PROBE_LOCK` (the probe may be called from several worker threads).
_PROBE_CACHE: dict[str, ScaledMmProbe] = {}
_PROBE_LOCK = threading.Lock()


def probe_scaled_mm(device: Any) -> ScaledMmProbe:
    """Run one rowwise `_scaled_mm` of `(128x256) @ (256x256)` on `device` and
    compare it with the `DEQUANT` path (relative L2 error <= 0.05).

    A DEFINITIVE answer is cached per `str(device)` for the process (a
    capability does not change while the process runs): a completed run
    (accepted or over the tolerance) and any failure other than running out of
    memory. An out-of-memory failure (`torch.OutOfMemoryError`, e.g. while
    another model fills the device) says nothing about the capability: it is
    returned with `transient=True` and NOT cached, so the next call probes
    again. Never raises: any failure is a `supported=False` answer carrying
    the reason.
    """
    key = str(device)
    with _PROBE_LOCK:
        cached = _PROBE_CACHE.get(key)
    if cached is not None:
        return cached
    probe = _run_scaled_mm_probe(key)
    if probe.transient:
        return probe
    with _PROBE_LOCK:
        # Another thread may have stored an answer meanwhile; keep the first.
        return _PROBE_CACHE.setdefault(key, probe)


def _run_scaled_mm_probe(device: str) -> ScaledMmProbe:
    """One uncached probe run on `device`; see `probe_scaled_mm`."""
    rows, k, n = _PROBE_SHAPE
    try:
        generator = torch.Generator().manual_seed(0)
        x = torch.randn(rows, k, generator=generator).to(torch.bfloat16)
        weight, scale = quantize_rows(torch.randn(n, k, generator=generator))
        layer = Fp8RowwiseLinear(k, n)
        layer.weight, layer.weight_scale = weight, scale
        layer = layer.to(device)
        x = x.to(device)
        layer.compute = Fp8Compute.DEQUANT
        reference = layer(x).float()
        layer.compute = Fp8Compute.SCALED_MM
        fast = layer(x).float()
        rel_error = float((fast - reference).norm() / reference.norm().clamp(min=1e-12))
    except torch.OutOfMemoryError as exc:
        reason = f"{type(exc).__name__}: {exc}"
        log.warning(
            "FLUX.3 klein: проверка rowwise _scaled_mm на %s не выполнена из-за нехватки памяти "
            "(результат не запомнен, следующая загрузка проверит снова): %s",
            device,
            reason,
        )
        return ScaledMmProbe(supported=False, reason=reason, rel_error=None, transient=True)
    except Exception as exc:  # noqa: BLE001 - the probe's contract: any failure is "unsupported", with the reason
        reason = f"{type(exc).__name__}: {exc}"
        log.info("FLUX.3 klein: rowwise _scaled_mm недоступен на %s: %s", device, reason)
        return ScaledMmProbe(supported=False, reason=reason, rel_error=None)
    if not math.isfinite(rel_error) or rel_error > _PROBE_MAX_REL_ERROR:
        reason = f"относительная ошибка {rel_error:.4f} больше допустимой {_PROBE_MAX_REL_ERROR}"
        log.info("FLUX.3 klein: rowwise _scaled_mm на %s отклонён: %s", device, reason)
        return ScaledMmProbe(supported=False, reason=reason, rel_error=rel_error)
    log.info("FLUX.3 klein: rowwise _scaled_mm работает на %s (относительная ошибка %.4f)", device, rel_error)
    return ScaledMmProbe(supported=True, reason="", rel_error=rel_error)


def resolve_fp8_compute(requested: Fp8Compute, device: Any) -> Fp8Compute:
    """Turn the requested fp8 path into a concrete one for `device`.

    `DEQUANT` stays. `SCALED_MM` is checked with `probe_scaled_mm`. `AUTO`
    becomes `SCALED_MM` when the probe passes, else `DEQUANT` with a WARNING
    naming the probe's reason (never silent).

    # Raises
    `RuntimeError` when `SCALED_MM` was requested explicitly and the probe fails.
    """
    if requested is Fp8Compute.DEQUANT:
        return Fp8Compute.DEQUANT
    probe = probe_scaled_mm(device)
    if probe.supported:
        return Fp8Compute.SCALED_MM
    if requested is Fp8Compute.SCALED_MM:
        raise RuntimeError(
            f"FLUX.3 klein: запрошено вычисление fp8 через _scaled_mm, но на {device} оно не работает: "
            f"{probe.reason}. Выберите «dequant» или «auto»."
        )
    log.warning(
        "FLUX.3 klein: fp8 через _scaled_mm на %s недоступен (%s); используется деквантизация в bf16",
        device,
        probe.reason,
    )
    return Fp8Compute.DEQUANT


def set_fp8_compute(module: nn.Module, mode: Fp8Compute) -> int:
    """Set `compute` of every `Fp8RowwiseLinear` in `module` to `mode`; returns how many.

    # Raises
    `ValueError` for `AUTO` (resolve it first with `resolve_fp8_compute`).
    """
    if mode is Fp8Compute.AUTO:
        raise ValueError("set_fp8_compute: AUTO нужно сначала разрешить через resolve_fp8_compute")
    switched = 0
    for layer in module.modules():
        if isinstance(layer, Fp8RowwiseLinear):
            layer.compute = mode
            switched += 1
    return switched


def swap_quantized_linears(module: nn.Module, keys: Collection[str]) -> WeightFormat:
    """Replace, in place, every `nn.Linear` whose scale key is in `keys` by the
    quantized layer of its format, built on the linear's device (`meta` for a
    loader). NVFP4 (`.weight_scale_2`) is checked first because its block scales
    are also named `.weight_scale`; only one format is ever applied.

    Returns `NVFP4`, `FP8R`, or `BF16` when no linear has a scale key.

    # Raises
    `ValueError` for a quantized linear that has a bias (the exports have none,
    and the quantized layers carry no bias).
    """
    for suffix, factory, weight_format in (
        (NVFP4_SCALE_SUFFIX, Nvfp4Linear, WeightFormat.NVFP4),
        (FP8_SCALE_SUFFIX, Fp8RowwiseLinear, WeightFormat.FP8R),
    ):
        names = [
            name
            for name, child in module.named_modules()
            if name and isinstance(child, nn.Linear) and f"{name}{suffix}" in keys
        ]
        if not names:
            continue
        for name in names:
            linear = module.get_submodule(name)
            if linear.bias is not None:
                raise ValueError(f"FLUX.3 klein: квантованный слой «{name}» имеет bias, такой формат не поддерживается")
            parent, _, child = name.rpartition(".")
            owner = module.get_submodule(parent) if parent else module
            setattr(owner, child, factory(linear.in_features, linear.out_features, linear.weight.device))
        log.debug("FLUX.3 klein: %d линейных слоёв заменены на %s", len(names), factory.__name__)
        return weight_format
    return WeightFormat.BF16
