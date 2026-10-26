"""
File: modules/ai_backend/runtime/_gguf_test_fixtures.py

Purpose:
Test-only writer of tiny, real GGUF files (through `gguf.GGUFWriter`) shared by
every GGUF test of the backend: the container seam (`test_gguf_stream`,
`test_tensor_container`, the filler / inventory / hooks tests) and the layers,
transformer and text-encoder packages built on it.

Main responsibilities:
- `FixtureTensor` - one tensor to write: name, ggml type, logical shape in
  TORCH order, the bytes as stored (`raw`: packed uint8 rows for a quantized
  type) and `reference()`, its values dequantized by `gguf.quants` (the
  independent reference a dequantizer is compared against);
- `make_tensor(name, qtype, shape, seed=)` - a seeded random tensor of any
  supported type: F32 / F16 / BF16 / Q8_0 / Q4_0 / Q5_0 quantized from random
  values by `gguf.quants.quantize`; Q4_K / Q5_K / Q6_K (which `gguf.quants`
  cannot quantize) as random blocks whose f16 scales are set to small finite
  values, so every dequantized value is finite;
- `write_gguf(path, tensors, *, architecture=, metadata=, alignment=)`;
- name sets for the two naming styles a consumer must handle:
  `bfl_style_tensors(seed=)` (FLUX BFL keys: `double_blocks.N...`,
  `single_blocks.N...`, `img_in.weight`, norm scales) and
  `llama_style_tensors(seed=)` (llama.cpp keys: `token_embd.weight` — a
  quantized 2-D embedding —, `blk.N.attn_q.weight`, `output_norm.weight`);
  `mixed_fixture_tensors(seed=)` covers every type at once.

Notes:
- Never imported by production code. Needs numpy and `gguf` (both in the
  backend venv); no torch. Callers write into a temp dir they remove.
- Shapes are torch order (`(out_features, in_features)` for a linear weight);
  the writer stores ggml order. A quantized type needs the LAST torch dim (the
  ggml row) to be a multiple of its block size (32, or 256 for K-quants).
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import numpy as np
from gguf import GGMLQuantizationType, GGUFWriter, quants
from gguf.constants import GGML_QUANT_SIZES

Q = GGMLQuantizationType

#: Types `gguf.quants.quantize` handles (plus F32 / F16 as numpy casts).
_QUANTIZABLE = frozenset({Q.F32, Q.F16, Q.BF16, Q.Q8_0, Q.Q4_0, Q.Q5_0})

#: K-quant block layouts (ggml `block_q*_K`): byte offsets of the f16 scales
#: that must be finite, everything else in the block is free bit patterns.
_K_SCALE_OFFSETS: dict[GGMLQuantizationType, tuple[int, ...]] = {
    Q.Q4_K: (0, 2),  # d, dmin, then scales[12], qs[128]
    Q.Q5_K: (0, 2),  # d, dmin, then scales[12], qh[32], qs[128]
    Q.Q6_K: (208,),  # ql[128], qh[64], scales[16] (int8), then d
}


@dataclass(frozen=True)
class FixtureTensor:
    """One tensor of a fixture file.

    `shape` is the logical shape in torch order; `raw` the stored bytes:
    float32 / float16 values for F32 / F16, and packed uint8 of shape
    `(*shape[:-1], bytes_per_row)` for BF16 and every quantized type (what
    `TensorSpan.shape` / `read_tensor` report for it).
    """

    name: str
    qtype: GGMLQuantizationType
    shape: tuple[int, ...]
    raw: np.ndarray

    @property
    def nbytes(self) -> int:
        """Stored bytes."""
        return int(self.raw.nbytes)

    def reference(self) -> np.ndarray:
        """The values as float32 in torch shape, dequantized by `gguf.quants` (independent of the backend)."""
        if self.qtype == Q.F32:
            return self.raw.astype(np.float32).reshape(self.shape)
        if self.qtype == Q.F16:
            return self.raw.astype(np.float32).reshape(self.shape)
        return quants.dequantize(self.raw, self.qtype).astype(np.float32).reshape(self.shape)


def bytes_per_row(qtype: GGMLQuantizationType, row_elements: int) -> int:
    """Stored bytes of one row of `row_elements` values (whole blocks).

    # Raises
    `ValueError` when `row_elements` is not a multiple of the block size.
    """
    block, type_size = GGML_QUANT_SIZES[qtype]
    if row_elements % block:
        raise ValueError(f"row of {row_elements} is not a multiple of the {qtype.name} block size {block}")
    return row_elements // block * type_size


def _random_k_blocks(rng: np.random.Generator, qtype: GGMLQuantizationType, shape: tuple[int, ...]) -> np.ndarray:
    """Random K-quant rows with small finite f16 scales (module notes)."""
    block, type_size = GGML_QUANT_SIZES[qtype]
    rows = int(np.prod(shape[:-1], dtype=np.int64)) if len(shape) > 1 else 1
    blocks = rows * (shape[-1] // block)
    data = rng.integers(0, 256, size=(blocks, type_size), dtype=np.uint8)
    for offset in _K_SCALE_OFFSETS[qtype]:
        scales = rng.uniform(0.001, 0.02, size=blocks).astype(np.float16)
        data[:, offset : offset + 2] = scales.view(np.uint8).reshape(blocks, 2)
    return data.reshape(*shape[:-1], bytes_per_row(qtype, shape[-1]))


def make_tensor(name: str, qtype: GGMLQuantizationType, shape: Sequence[int], *, seed: int = 0) -> FixtureTensor:
    """A seeded random `FixtureTensor` of `qtype` with torch-order `shape`.

    # Raises
    `ValueError` for a type this writer cannot produce or a row that is not
    whole blocks.
    """
    logical = tuple(int(dim) for dim in shape)
    rng = np.random.default_rng(seed)
    bytes_per_row(qtype, logical[-1])
    if qtype in _K_SCALE_OFFSETS:
        raw = _random_k_blocks(rng, qtype, logical)
    elif qtype in _QUANTIZABLE:
        values = rng.standard_normal(logical).astype(np.float32)
        raw = quants.quantize(values, qtype)
    else:
        raise ValueError(f"the fixture writer cannot produce {qtype.name}")
    return FixtureTensor(name=name, qtype=qtype, shape=logical, raw=np.ascontiguousarray(raw))


def write_gguf(
    path: Path,
    tensors: Sequence[FixtureTensor],
    *,
    architecture: str = "flux",
    metadata: Mapping[str, Any] | None = None,
    alignment: int | None = None,
) -> Path:
    """Write `tensors` (in the given order) to a GGUF v3 file at `path`.

    `metadata` adds key/values: `str`, `int` (written as uint32), `float`
    (float32), `bool` and lists of `str` (an array, which the backend's parser
    skips). `alignment` sets `general.alignment` (default: the format's 32).
    """
    writer = GGUFWriter(path, architecture)
    if alignment is not None:
        writer.add_custom_alignment(alignment)
    for key, value in (metadata or {}).items():
        if isinstance(value, bool):
            writer.add_bool(key, value)
        elif isinstance(value, int):
            writer.add_uint32(key, value)
        elif isinstance(value, float):
            writer.add_float32(key, value)
        elif isinstance(value, str):
            writer.add_string(key, value)
        elif isinstance(value, list):
            writer.add_array(key, value)
        else:
            raise TypeError(f"unsupported fixture metadata value for {key!r}: {type(value).__name__}")
    for tensor in tensors:
        if tensor.qtype in (Q.F32, Q.F16):
            writer.add_tensor(tensor.name, tensor.raw)
        else:
            writer.add_tensor(tensor.name, tensor.raw, raw_dtype=tensor.qtype)
    writer.write_header_to_file()
    writer.write_kv_data_to_file()
    writer.write_tensors_to_file()
    writer.close()
    return path


def bfl_style_tensors(*, seed: int = 0, width: int = 256) -> list[FixtureTensor]:
    """FLUX BFL-style keys: quantized 2-D linears of every K / Q8_0 type, a BF16 linear, F32 norm scales, an F16 bias."""
    return [
        make_tensor("img_in.weight", Q.BF16, (width, 64), seed=seed),
        make_tensor("double_blocks.0.img_attn.qkv.weight", Q.Q8_0, (3 * width, width), seed=seed + 1),
        make_tensor("double_blocks.0.img_attn.proj.weight", Q.Q4_K, (width, width), seed=seed + 2),
        make_tensor("double_blocks.0.img_attn.norm.query_norm.scale", Q.F32, (32,), seed=seed + 3),
        make_tensor("double_blocks.0.img_mlp.0.weight", Q.Q5_K, (2 * width, width), seed=seed + 4),
        make_tensor("single_blocks.0.linear1.weight", Q.Q6_K, (2 * width, width), seed=seed + 5),
        make_tensor("single_blocks.0.linear1.bias", Q.F16, (2 * width,), seed=seed + 6),
        make_tensor("single_blocks.0.norm.key_norm.scale", Q.F32, (32,), seed=seed + 7),
    ]


def llama_style_tensors(*, seed: int = 100, hidden: int = 256, vocab: int = 48) -> list[FixtureTensor]:
    """llama.cpp-style keys: a quantized 2-D embedding (`token_embd.weight`), quantized attention / MLP linears, F32 norms."""
    return [
        make_tensor("token_embd.weight", Q.Q6_K, (vocab, hidden), seed=seed),
        make_tensor("blk.0.attn_norm.weight", Q.F32, (hidden,), seed=seed + 1),
        make_tensor("blk.0.attn_q.weight", Q.Q4_K, (hidden, hidden), seed=seed + 2),
        make_tensor("blk.0.attn_k.weight", Q.Q8_0, (hidden // 2, hidden), seed=seed + 3),
        make_tensor("blk.0.attn_q_norm.weight", Q.F32, (32,), seed=seed + 4),
        make_tensor("blk.0.ffn_down.weight", Q.Q5_K, (hidden, 2 * hidden), seed=seed + 5),
        make_tensor("output_norm.weight", Q.F32, (hidden,), seed=seed + 6),
    ]


def mixed_fixture_tensors(*, seed: int = 0) -> list[FixtureTensor]:
    """Every type the writer produces in one file: both naming styles plus a Q4_0 / Q5_0 pair."""
    return [
        *bfl_style_tensors(seed=seed),
        *llama_style_tensors(seed=seed + 100),
        make_tensor("extra.q4_0.weight", Q.Q4_0, (4, 64), seed=seed + 200),
        make_tensor("extra.q5_0.weight", Q.Q5_0, (4, 64), seed=seed + 201),
    ]
