"""
File: modules/ai_backend/runtime/gguf_layers.py

Purpose:
Torch layers that keep a GGUF-quantized weight quantized in memory and
dequantize it per forward: the model-agnostic building blocks a model package
swaps into a module built on `meta` before the large-model loader fills it from
a GGUF file.

Key structures:
- `GgufLinear` - `nn.Linear` replacement over a quantized weight.
- `GgufEmbedding` - `nn.Embedding` replacement that dequantizes only the rows
  it looks up.

Key functions:
- `dequantizable_types()` / `require_dequantizable()` - the supported ggml types
  and the one refusal of every other type.
- `bytes_per_row()` - the stored row length of a quantized weight.
- `dequantize()` - packed `uint8` rows -> float rows.
- `swap_gguf_modules()` - replace the `nn.Linear` / `nn.Embedding` submodules a
  GGUF header marks quantized.

Notes:
- The weight is a PERSISTENT `uint8` buffer named `weight` with the file's byte
  layout `(rows, bytes_per_row)`, and the quant type lives on the MODULE
  (`quant_type`), never on the tensor (no tensor subclass). So the weight
  survives everything the large-model manager does to a state entry: a
  `TensorSlot.set` of a plain tensor, a packed host-buffer or device-slot view
  (`packed_view`), a `meta` placeholder, a `state_dict` round trip.
- Dequantization uses ONLY diffusers' pure-torch block functions
  (`diffusers.quantizers.gguf.utils.dequantize_functions`, called with
  `dtype=None`), never its `ops` / `forward_cuda` path: that path fetches a
  CUDA kernel from the network and is wrong on ROCm (see
  `rocm_runtime._drop_gguf_cuda_kernels_flag`).
- Imports torch and diffusers at module top: a torch-free module may import
  this one only lazily, inside the function that needs it.
"""

from __future__ import annotations

import logging
from collections.abc import Mapping
from typing import Any

import gguf
import torch
import torch.nn.functional as F
from diffusers.quantizers.gguf.utils import GGML_QUANT_SIZES, dequantize_functions
from torch import Tensor, nn

log = logging.getLogger(__name__)

#: Suffix every `swap_gguf_modules` key carries (`"<module path>.weight"`).
_WEIGHT_SUFFIX = ".weight"


def dequantizable_types() -> frozenset[int]:
    """The ggml type ids `dequantize` supports: exactly diffusers' block-function table."""
    return frozenset(int(quant_type) for quant_type in dequantize_functions)


def quant_type_name(quant_type: int) -> str:
    """The ggml name of `quant_type` (`"Q4_K"`), or `"unknown(<id>)"` for an id gguf does not know."""
    try:
        return gguf.GGMLQuantizationType(quant_type).name
    except ValueError:
        return f"unknown({quant_type})"


def require_dequantizable(quant_type: int, *, where: str) -> None:
    """Raise `ValueError` unless `quant_type` is dequantizable.

    The message names `where` (the tensor or module the type belongs to), the
    type by name and id, and the supported list, so a refused file says which
    tensor of which quant it could not use.
    """
    if int(quant_type) in dequantizable_types():
        return
    supported = ", ".join(sorted(quant_type_name(qt) for qt in dequantizable_types()))
    raise ValueError(
        f"{where}: GGUF quantization type {quant_type_name(quant_type)} (id {quant_type}) "
        f"is not supported; supported types: {supported}"
    )


def bytes_per_row(quant_type: int, row_elements: int) -> int:
    """Stored bytes of one row of `row_elements` values in `quant_type`.

    Raises `ValueError` when the type is not dequantizable or `row_elements` is
    not a positive multiple of the type's block size (a GGUF row always holds
    whole blocks).
    """
    require_dequantizable(quant_type, where="bytes_per_row")
    block_size, type_size = GGML_QUANT_SIZES[quant_type]
    if row_elements <= 0 or row_elements % block_size:
        raise ValueError(
            f"GGUF row of {row_elements} elements is not a positive multiple of the "
            f"{quant_type_name(quant_type)} block size {block_size}"
        )
    return row_elements // block_size * type_size


def dequantize(rows_u8: Tensor, quant_type: int, *, row_elements: int) -> Tensor:
    """Dequantize packed rows `(*rows, bytes_per_row)` of `uint8` into `(*rows, row_elements)`.

    The result keeps the block function's own dtype (`float16` for the
    fp16-scaled quants, `float32` for BF16) on the input's device; the caller
    casts. It is bit-identical to the weight diffusers'
    `GGUFLinear.forward_native` computes before its `.to(compute_dtype)`.
    Raises `ValueError` for a non-`uint8` input, an unsupported type or a last
    dimension that does not match `bytes_per_row(quant_type, row_elements)`.
    """
    if rows_u8.dtype != torch.uint8:
        raise ValueError(f"GGUF rows must be uint8, got {rows_u8.dtype}")
    expected = bytes_per_row(quant_type, row_elements)
    if rows_u8.dim() < 1 or rows_u8.shape[-1] != expected:
        raise ValueError(
            f"GGUF rows of shape {tuple(rows_u8.shape)} do not end in {expected} bytes "
            f"({row_elements} {quant_type_name(quant_type)} elements)"
        )
    block_size, type_size = GGML_QUANT_SIZES[quant_type]
    # The same flattening as diffusers' `dequantize_gguf_tensor`: whole blocks
    # in row-major order, so the result reshapes back onto the row layout.
    blocks = rows_u8.reshape(-1, type_size)
    values = dequantize_functions[quant_type](blocks, block_size, type_size, None)
    return values.reshape(*rows_u8.shape[:-1], row_elements)


class GgufLinear(nn.Module):
    """`nn.Linear` over a GGUF-quantized weight, dequantized per forward.

    Buffers: `weight` uint8 `(out_features, bytes_per_row(quant_type, in_features))`,
    persistent. Parameter: `bias` `(out_features,)` in `compute_dtype` when
    `bias` is true. Storage is uninitialized until the loader fills it. The
    forward is `F.linear(x, dequantize(weight).to(compute_dtype), bias)`; the
    input must already be in `compute_dtype`. Raises `ValueError` at build for
    an unsupported type or an `in_features` that is not whole blocks.
    """

    def __init__(
        self,
        in_features: int,
        out_features: int,
        *,
        quant_type: int,
        compute_dtype: torch.dtype,
        bias: bool,
        device: Any = None,
    ) -> None:
        super().__init__()
        row_bytes = bytes_per_row(quant_type, in_features)
        self.in_features, self.out_features = in_features, out_features
        self.quant_type = int(quant_type)
        self.compute_dtype = compute_dtype
        self.register_buffer("weight", torch.empty(out_features, row_bytes, dtype=torch.uint8, device=device))
        if bias:
            self.bias = nn.Parameter(torch.empty(out_features, dtype=compute_dtype, device=device), requires_grad=False)
        else:
            self.register_parameter("bias", None)

    def extra_repr(self) -> str:
        return (
            f"in_features={self.in_features}, out_features={self.out_features}, "
            f"quant_type={quant_type_name(self.quant_type)}, bias={self.bias is not None}"
        )

    def forward(self, x: Tensor) -> Tensor:
        """`x [..., in_features]` -> `[..., out_features]` through a transient dequantized weight."""
        weight = dequantize(self.weight, self.quant_type, row_elements=self.in_features).to(self.compute_dtype)
        return F.linear(x, weight, self.bias)


class GgufEmbedding(nn.Module):
    """`nn.Embedding` over a GGUF-quantized table that dequantizes only the looked-up rows.

    Buffers: `weight` uint8 `(num_embeddings, bytes_per_row(quant_type, embedding_dim))`,
    persistent, uninitialized until the loader fills it. The forward selects the
    packed rows of `ids` (repeats included), dequantizes those rows only and
    returns `(*ids.shape, embedding_dim)` in `compute_dtype`: the full table is
    never dequantized. `padding_idx` is kept for parity with the replaced
    module (it only affects gradients, which this inference layer never has).
    """

    def __init__(
        self,
        num_embeddings: int,
        embedding_dim: int,
        *,
        quant_type: int,
        compute_dtype: torch.dtype,
        padding_idx: int | None = None,
        device: Any = None,
    ) -> None:
        super().__init__()
        row_bytes = bytes_per_row(quant_type, embedding_dim)
        self.num_embeddings, self.embedding_dim = num_embeddings, embedding_dim
        self.quant_type = int(quant_type)
        self.compute_dtype = compute_dtype
        self.padding_idx = padding_idx
        self.register_buffer("weight", torch.empty(num_embeddings, row_bytes, dtype=torch.uint8, device=device))

    def extra_repr(self) -> str:
        return (
            f"num_embeddings={self.num_embeddings}, embedding_dim={self.embedding_dim}, "
            f"quant_type={quant_type_name(self.quant_type)}, padding_idx={self.padding_idx}"
        )

    def forward(self, ids: Tensor) -> Tensor:
        """Integer `ids` of any shape -> `(*ids.shape, embedding_dim)` in `compute_dtype`."""
        rows = self.weight.index_select(0, ids.reshape(-1))
        values = dequantize(rows, self.quant_type, row_elements=self.embedding_dim)
        return values.reshape(*ids.shape, self.embedding_dim).to(self.compute_dtype)


def _build_replacement(module: nn.Module, key: str, quant_type: int, compute_dtype: torch.dtype) -> tuple[str, str, nn.Module]:
    """Validate one `swap_gguf_modules` key; return `(parent path, leaf name, replacement)`.

    Raises `ValueError` naming `key` for every refusal (see `swap_gguf_modules`).
    """
    if not key.endswith(_WEIGHT_SUFFIX) or len(key) == len(_WEIGHT_SUFFIX):
        raise ValueError(f"GGUF quantized key {key!r} does not name a submodule weight ('<path>.weight')")
    path = key[: -len(_WEIGHT_SUFFIX)]
    try:
        target = module.get_submodule(path)
    except AttributeError as exc:
        raise ValueError(f"GGUF quantized key {key!r}: the model has no submodule {path!r}") from exc
    require_dequantizable(quant_type, where=key)
    device = target.weight.device
    if isinstance(target, nn.Linear):
        if target.in_features % GGML_QUANT_SIZES[quant_type][0]:
            raise ValueError(
                f"GGUF quantized key {key!r}: in_features={target.in_features} is not a multiple of the "
                f"{quant_type_name(quant_type)} block size {GGML_QUANT_SIZES[quant_type][0]}"
            )
        replacement: nn.Module = GgufLinear(
            target.in_features,
            target.out_features,
            quant_type=quant_type,
            compute_dtype=compute_dtype,
            bias=target.bias is not None,
            device=device,
        )
    elif isinstance(target, nn.Embedding):
        if target.embedding_dim % GGML_QUANT_SIZES[quant_type][0]:
            raise ValueError(
                f"GGUF quantized key {key!r}: embedding_dim={target.embedding_dim} is not a multiple of the "
                f"{quant_type_name(quant_type)} block size {GGML_QUANT_SIZES[quant_type][0]}"
            )
        # `max_norm` renormalizes the table in place on lookup, which a packed
        # quantized table cannot do: refuse rather than silently drop it.
        if target.max_norm is not None:
            raise ValueError(f"GGUF quantized key {key!r}: an nn.Embedding with max_norm is not supported")
        replacement = GgufEmbedding(
            target.num_embeddings,
            target.embedding_dim,
            quant_type=quant_type,
            compute_dtype=compute_dtype,
            padding_idx=target.padding_idx,
            device=device,
        )
    else:
        raise ValueError(
            f"GGUF quantized key {key!r}: {type(target).__name__} cannot hold a quantized weight "
            "(only nn.Linear and nn.Embedding)"
        )
    parent, _, leaf = path.rpartition(".")
    return parent, leaf, replacement


def swap_gguf_modules(module: nn.Module, quant_types: Mapping[str, int], *, compute_dtype: torch.dtype) -> int:
    """Replace every submodule a GGUF header marks quantized; return how many were replaced.

    `quant_types` maps `"<module path>.weight"` to its ggml type id. Each
    `nn.Linear` becomes a `GgufLinear` and each `nn.Embedding` a
    `GgufEmbedding`, built on the target's own device (`meta` for the loader)
    with uninitialized storage: the caller fills it from the file. All keys are
    validated before any module is replaced, so a refusal leaves `module`
    untouched. Raises `ValueError` for a key that names no submodule, a target
    that is neither linear nor embedding, a row length that is not whole
    blocks, or a type that is not dequantizable.
    """
    planned = [_build_replacement(module, key, int(quant_type), compute_dtype) for key, quant_type in quant_types.items()]
    for parent_path, leaf, replacement in planned:
        setattr(module.get_submodule(parent_path), leaf, replacement)
    log.debug("swap_gguf_modules: replaced %d submodule(s) with GGUF layers (compute %s)", len(planned), compute_dtype)
    return len(planned)
