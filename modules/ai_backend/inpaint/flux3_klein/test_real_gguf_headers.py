"""
File: modules/ai_backend/inpaint/flux3_klein/test_real_gguf_headers.py

Purpose:
Opt-in check of the real FLUX.3 klein GGUF files against this port:
- every `transformer_gguf/*.gguf` header passes `loading.require_gguf_dit`
  against the package root's `config.json`, and its quantized layers swap
  into a meta `Flux3Dit`;
- every `text_encoder_gguf/*.gguf` passes `text_encoder.require_gguf_text_encoder`
  against `text_encoder/config.json`;
- a Q8_0 file of each component holds the bf16 weights: a few rows of a few
  tensors, read by BYTE RANGE from the GGUF and from the bf16 checkpoint
  (`model.safetensors`, the `text_encoder/` shards), dequantized
  (`runtime.gguf_layers.dequantize`) and compared (correlation and relative
  error), which pins the name bridge and "no Q/K permutation" on real data.

Notes:
Skipped unless `MS_FLUX3_DIR` names the package root (the catalog layout:
`transformer_gguf/`, `text_encoder_gguf/` next to `model.safetensors` and
`text_encoder/`), and per file when absent. Reads headers plus a few KiB of
rows: no tensor is loaded whole, no GPU is used.
"""

from __future__ import annotations

import json
import os
import unittest
from pathlib import Path

import torch

from ...runtime.gguf_layers import dequantize, swap_gguf_modules
from ...runtime.safetensors_stream import TensorSpan, parse_tensor_spans, read_safetensors_header_and_data_start
from ...runtime.tensor_container import ContainerLayout, read_container_layout
from . import loading
from . import text_encoder as te
from .config import read_package_config
from .dit import Flux3Dit

_ROOT_ENV = "MS_FLUX3_DIR"

#: Rows compared per tensor (spread over the tensor).
_ROWS = 8

#: DiT tensors compared (state key = GGUF name = safetensors key).
_DIT_KEYS = ("content_mode_blocks.image.0.k_proj.weight", "txt_mode_blocks.3.mlp_in.weight", "single_blocks.5.mlp_out.weight")

#: Encoder tensors compared: GGUF name -> bf16 shard key (the bridge's answer, written out by hand).
_ENCODER_KEYS = {
    "blk.0.attn_q.weight": "model.language_model.layers.0.self_attn.q_proj.weight",
    "blk.5.attn_k.weight": "model.language_model.layers.5.self_attn.k_proj.weight",
    "blk.5.ffn_down.weight": "model.language_model.layers.5.mlp.down_proj.weight",
    "token_embd.weight": "model.language_model.embed_tokens.weight",
}


def _rows_gguf(path: Path, layout: ContainerLayout, span: TensorSpan, rows: list[int]) -> torch.Tensor:
    """`rows` of the 2-D GGUF tensor `span`, read by byte range and dequantized to float32."""
    row_bytes = span.shape[-1]
    out = []
    with path.open("rb") as handle:
        for row in rows:
            handle.seek(layout.data_start + span.start + row * row_bytes)
            data = torch.frombuffer(bytearray(handle.read(row_bytes)), dtype=torch.uint8)
            if span.quant_type is None:
                raise AssertionError(f"{span.name} is not quantized")
            out.append(dequantize(data[None], int(span.quant_type), row_elements=_elements(span)).float()[0])
    return torch.stack(out)


def _elements(span: TensorSpan) -> int:
    """Elements per row of a quantized span (its ggml row length)."""
    from diffusers.quantizers.gguf.utils import GGML_QUANT_SIZES

    block_size, type_size = GGML_QUANT_SIZES[int(span.quant_type)]  # type: ignore[arg-type]
    return span.shape[-1] // type_size * block_size


def _rows_bf16(path: Path, key: str, rows: list[int]) -> torch.Tensor:
    """`rows` of the 2-D bf16 safetensors tensor `key` in `path`, read by byte range, as float32."""
    header, data_start = read_safetensors_header_and_data_start(path)
    span = next(span for span in parse_tensor_spans(header, source=path) if span.name == key)
    if span.dtype_token != "BF16" or len(span.shape) != 2:
        raise AssertionError(f"{key}: {span.dtype_token}{list(span.shape)} is not a 2-D bf16 tensor")
    row_bytes = span.shape[1] * 2
    out = []
    with path.open("rb") as handle:
        for row in rows:
            handle.seek(data_start + span.start + row * row_bytes)
            out.append(torch.frombuffer(bytearray(handle.read(row_bytes)), dtype=torch.bfloat16).float())
    return torch.stack(out)


class RealGgufTests(unittest.TestCase):
    def setUp(self) -> None:
        value = os.environ.get(_ROOT_ENV)
        if not value:
            self.skipTest(f"{_ROOT_ENV} is not set")
        self.root = Path(value)
        if not self.root.is_dir():
            self.skipTest(f"{_ROOT_ENV}={value} is not a directory")

    def _files(self, folder: str) -> list[Path]:
        files = sorted((self.root / folder).glob("*.gguf"))
        if not files:
            self.skipTest(f"no GGUF under {self.root / folder}")
        return files

    def _compare(self, gguf: torch.Tensor, bf16: torch.Tensor, label: str) -> None:
        correlation = torch.corrcoef(torch.stack([gguf.flatten(), bf16.flatten()]))[0, 1].item()
        relative = ((gguf - bf16).norm() / bf16.norm()).item()
        self.assertGreater(correlation, 0.999, label)
        self.assertLess(relative, 0.02, label)

    def test_every_dit_header_fits_the_root_config(self) -> None:
        package = read_package_config(self.root / "config.json")
        for path in self._files("transformer_gguf"):
            with self.subTest(file=path.name):
                layout = loading.gguf_dit_layout(path, format_id="probe")
                quant_types = loading.require_gguf_dit(layout.spans, package, source=path, format_id="probe")
                with torch.device("meta"):
                    dit = Flux3Dit(package.dit)
                self.assertEqual(swap_gguf_modules(dit, quant_types, compute_dtype=torch.bfloat16), len(quant_types))

    def test_every_encoder_header_fits_the_package_encoder(self) -> None:
        for path in self._files("text_encoder_gguf"):
            with self.subTest(file=path.name):
                te.require_gguf_text_encoder(path, self.root / "text_encoder", format_id="probe")

    def test_q8_0_dit_rows_equal_the_bf16_rows(self) -> None:
        path = next((file for file in self._files("transformer_gguf") if file.name.endswith("Q8_0.gguf")), None)
        bf16 = self.root / "model.safetensors"
        if path is None or not bf16.is_file():
            self.skipTest("no Q8_0 DiT or no bf16 model.safetensors")
        layout = read_container_layout(path)
        spans = {span.name: span for span in layout.spans}
        for key in _DIT_KEYS:
            with self.subTest(key=key):
                span = spans[key]
                rows = [index * (span.shape[0] // _ROWS) for index in range(_ROWS)]
                self._compare(_rows_gguf(path, layout, span, rows), _rows_bf16(bf16, key, rows), key)

    def test_q8_0_encoder_rows_equal_the_bf16_shard_rows(self) -> None:
        path = next((file for file in self._files("text_encoder_gguf") if file.name.endswith("Q8_0.gguf")), None)
        index_file = self.root / "text_encoder" / "model.safetensors.index.json"
        if path is None or not index_file.is_file():
            self.skipTest("no Q8_0 encoder or no bf16 shards")
        weight_map = json.loads(index_file.read_text(encoding="utf-8"))["weight_map"]
        layout = read_container_layout(path)
        spans = {span.name: span for span in layout.spans}
        for name, key in _ENCODER_KEYS.items():
            with self.subTest(name=name):
                span = spans[name]
                rows = [index * (span.shape[0] // _ROWS) for index in range(_ROWS)]
                shard = self.root / "text_encoder" / weight_map[key]
                self._compare(_rows_gguf(path, layout, span, rows), _rows_bf16(shard, key, rows), name)


if __name__ == "__main__":
    unittest.main()
