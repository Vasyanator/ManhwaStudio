"""
File: modules/ai_backend/runtime/test_tensor_container.py

Purpose:
Unit tests for `runtime/tensor_container.py`, the one container dispatch:
`container_kind` by magic, `read_container_layout` for both containers (spans,
data start, file size, the GGUF header kept; a header view that leaves the
file-size bound to the reader), `open_tensor_reader` returning the right reader with the exact bytes of
GGUF quantized rows and plain tensors, and that importing the module and
parsing a GGUF layout never imports torch.

Notes:
The GGUF cases need the `gguf` package, the byte comparisons torch; each class
skips itself without what it needs. Files go into a temp dir removed by the test.
"""

from __future__ import annotations

import importlib.util
import json
import struct
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from . import safetensors_stream as ss
from . import tensor_container as tc

_HAS_GGUF = importlib.util.find_spec("gguf") is not None
_HAS_TORCH = importlib.util.find_spec("torch") is not None
_PROJECT_ROOT = Path(__file__).resolve().parents[3]


def _write_safetensors(path: Path, header: dict[str, object], data: bytes) -> Path:
    payload = json.dumps(header).encode("utf-8")
    path.write_bytes(struct.pack("<Q", len(payload)) + payload + data)
    return path


class _TempCase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)
        self.st = _write_safetensors(
            self.root / "a.safetensors",
            {"w": {"dtype": "F32", "shape": [2], "data_offsets": [0, 8]}},
            struct.pack("<2f", 1.0, -2.5),
        )


class SafetensorsDispatchTests(_TempCase):
    def test_a_safetensors_file_is_dispatched_to_its_parser(self) -> None:
        self.assertEqual(tc.container_kind(self.st), "safetensors")
        layout = tc.read_container_layout(self.st)
        self.assertEqual(layout.kind, "safetensors")
        self.assertIsNone(layout.gguf)
        header, data_start = ss.read_safetensors_header_and_data_start(self.st)
        self.assertEqual(layout.data_start, data_start)
        self.assertEqual(layout.file_size, self.st.stat().st_size)
        self.assertEqual(layout.spans, tuple(ss.parse_tensor_spans(header, source=self.st)))
        self.assertIsInstance(tc.open_tensor_reader(self.st), ss.StreamingSafetensorsReader)

    def test_the_layout_is_a_header_view_and_the_reader_checks_the_size(self) -> None:
        short = _write_safetensors(self.root / "short.safetensors", {"w": {"dtype": "F32", "shape": [4], "data_offsets": [0, 16]}}, b"\0" * 8)
        self.assertEqual(tc.read_container_layout(short).spans[0].nbytes, 16)
        with self.assertRaises(ValueError) as caught:
            tc.open_tensor_reader(short)
        self.assertIn("Обрезанный safetensors", str(caught.exception))

    def test_an_unreadable_file_is_a_value_error(self) -> None:
        with self.assertRaises(ValueError):
            tc.container_kind(self.root / "missing.gguf")


@unittest.skipUnless(_HAS_GGUF, "gguf is not installed")
class GgufDispatchTests(_TempCase):
    def setUp(self) -> None:
        super().setUp()
        from . import _gguf_test_fixtures as fx

        self.tensors = fx.mixed_fixture_tensors(seed=11)
        self.path = fx.write_gguf(self.root / "m.gguf", self.tensors)

    def test_a_gguf_file_is_dispatched_to_its_parser(self) -> None:
        from .gguf_stream import read_gguf_header

        self.assertEqual(tc.container_kind(self.path), "gguf")
        layout = tc.read_container_layout(self.path)
        header = read_gguf_header(self.path)
        self.assertEqual(layout.kind, "gguf")
        self.assertEqual(layout.gguf, header)
        self.assertEqual((layout.spans, layout.data_start), (header.spans, header.data_start))
        self.assertEqual(layout.file_size, self.path.stat().st_size)
        reader = tc.open_tensor_reader(self.path)
        self.assertIs(type(reader), ss.StreamingTensorReader)
        self.assertEqual(reader.keys_in_file_order(), [span.name for span in header.spans])
        self.assertEqual(reader.total_tensor_bytes, sum(tensor.nbytes for tensor in self.tensors))

    def test_a_truncated_gguf_is_refused_by_the_reader_not_the_layout(self) -> None:
        cut = self.root / "cut.gguf"
        cut.write_bytes(self.path.read_bytes()[:-100])
        self.assertEqual(tc.read_container_layout(cut).spans, tc.read_container_layout(self.path).spans)
        with self.assertRaises(ValueError) as caught:
            tc.open_tensor_reader(cut)
        self.assertIn(f"Обрезанный GGUF {cut}", str(caught.exception))

    @unittest.skipUnless(_HAS_TORCH, "torch is not installed")
    def test_the_reader_returns_the_exact_bytes(self) -> None:
        import numpy as np
        import torch

        reader = tc.open_tensor_reader(self.path)
        spans = {span.name: span for span in reader.spans}
        with reader:
            for tensor in self.tensors:
                with self.subTest(tensor.name):
                    value = reader.read_tensor(spans[tensor.name])
                    self.assertEqual(tuple(value.shape), spans[tensor.name].shape)
                    stored = value.contiguous().view(torch.uint8).numpy().reshape(-1)
                    self.assertTrue(np.array_equal(stored, tensor.raw.reshape(-1).view(np.uint8)))
                    if tensor.qtype.name == "BF16":
                        self.assertEqual(value.dtype, torch.bfloat16)
                        self.assertTrue(np.array_equal(value.float().numpy(), tensor.reference()))
                    elif spans[tensor.name].quant_type is not None:
                        self.assertEqual(value.dtype, torch.uint8)


def test_importing_and_parsing_a_gguf_layout_never_imports_torch() -> None:
    """`tensor_container` (and the GGUF parse) must stay torch-free: the inventory runs it for sizing."""
    with tempfile.TemporaryDirectory() as scratch:
        _assert_torch_free(Path(scratch))


def _assert_torch_free(tmp_path: Path) -> None:
    """Run the import + parse in a fresh interpreter and assert torch never appears in `sys.modules`."""
    code = (
        "import sys, importlib.util\n"
        f"sys.path.insert(0, {str(_PROJECT_ROOT)!r})\n"
        "import modules.ai_backend.runtime.tensor_container as tc\n"
        "assert 'torch' not in sys.modules, 'tensor_container imported torch'\n"
        "if importlib.util.find_spec('gguf') is not None:\n"
        "    from modules.ai_backend.runtime import _gguf_test_fixtures as fx\n"
        f"    path = fx.write_gguf(__import__('pathlib').Path({str(tmp_path)!r}) / 'x.gguf', fx.llama_style_tensors())\n"
        "    tc.read_container_layout(path)\n"
        "    assert 'torch' not in sys.modules, 'the GGUF parse imported torch'\n"
    )
    result = subprocess.run([sys.executable, "-c", code], capture_output=True, text=True, timeout=120, check=False)
    assert result.returncode == 0, result.stderr


if __name__ == "__main__":
    unittest.main()
