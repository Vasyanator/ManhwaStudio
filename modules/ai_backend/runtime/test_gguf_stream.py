"""
File: modules/ai_backend/runtime/test_gguf_stream.py

Purpose:
Unit tests for `runtime/gguf_stream.py`: the header round trip on files written
by `gguf.GGUFWriter` (`_gguf_test_fixtures`: F32 / F16 / BF16 and Q8_0 / Q4_0 /
Q5_0 / Q4_K / Q5_K / Q6_K tensors, BFL- and llama.cpp-style names, a quantized
embedding) — torch-order shapes, U8 rows of whole blocks with `quant_type`,
offsets and data start against `gguf.GGUFReader`, kept scalar metadata and
skipped arrays, a custom alignment —, the policy-free IQ parse, zero-element
tensors, the magic sniff, type naming, and every malformed-header refusal on
hand-built files (bad magic, v1 / v4, truncation, an overflowing string length,
misaligned / overlapping spans (past-EOF spans are the reader's), a row that is not whole blocks, an
unknown type id, a dtype outside the table, duplicate names and keys, a bad
alignment, dimension counts), the count preflight (enormous tensor / key / array /
nested-array counts on compact or sparse files refused in well under a second,
the per-array cap, the whole-header item budget, a real-sized vocabulary still
parsing) plus the missing-package error.

Notes:
Torch-free. Needs the `gguf` package (skipped without it, except the
missing-package test). Every file is written into a temporary directory removed
by the test.
"""

from __future__ import annotations

import importlib.util
import struct
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import patch

from . import gguf_stream as gs
from .safetensors_stream import SAFETENSORS_DTYPES

_HAS_GGUF = importlib.util.find_spec("gguf") is not None

# GGUF metadata value type ids used by the raw builder.
_UINT32, _STRING, _ARRAY = 4, 8, 9
# ggml type ids used by the raw builder (stable ggml ABI numbers).
_F32, _F16, _Q8_0, _Q4_K = 0, 1, 8, 12


def _gguf_string(text: str | bytes) -> bytes:
    raw = text.encode("utf-8") if isinstance(text, str) else text
    return struct.pack("<Q", len(raw)) + raw


def _raw_gguf(
    *,
    tensors: list[tuple[str, list[int], int, int]],
    data: bytes = b"",
    kvs: bytes = b"",
    kv_count: int = 0,
    version: int = 3,
    magic: bytes = b"GGUF",
    alignment: int = 32,
) -> bytes:
    """A GGUF file built byte by byte: header, `kvs` (already encoded), tensor infos (ggml-order dims), padding, `data`."""
    body = magic + struct.pack("<IQQ", version, len(tensors), kv_count) + kvs
    for name, dims, type_id, offset in tensors:
        body += _gguf_string(name) + struct.pack("<I", len(dims)) + b"".join(struct.pack("<Q", d) for d in dims)
        body += struct.pack("<IQ", type_id, offset)
    body += b"\0" * ((-len(body)) % alignment)
    return body + data


def _kv_u32(key: str, value: int) -> bytes:
    return _gguf_string(key) + struct.pack("<II", _UINT32, value)


class _TempCase(unittest.TestCase):
    """A `TestCase` with a temp directory that is removed afterwards."""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)

    def _file(self, payload: bytes, name: str = "x.gguf") -> Path:
        path = self.root / name
        path.write_bytes(payload)
        return path


@unittest.skipUnless(_HAS_GGUF, "gguf is not installed")
class RoundTripTests(_TempCase):
    def setUp(self) -> None:
        super().setUp()
        from . import _gguf_test_fixtures as fx

        self.fx = fx
        self.tensors = fx.mixed_fixture_tensors(seed=3)
        self.path = fx.write_gguf(
            self.root / "mixed.gguf",
            self.tensors,
            metadata={"test.count": 7, "test.name": "tiny", "test.ratio": 0.5, "test.flag": True, "test.vocab": ["a", "b", "c"]},
        )

    def test_spans_match_the_writer_in_torch_order(self) -> None:
        header = gs.read_gguf_header(self.path)
        spans = {span.name: span for span in header.spans}
        self.assertEqual(set(spans), {tensor.name for tensor in self.tensors})
        for tensor in self.tensors:
            with self.subTest(tensor.name):
                span = spans[tensor.name]
                self.assertEqual(span.nbytes, tensor.nbytes)
                if tensor.qtype.name in ("F32", "F16", "BF16"):
                    self.assertIsNone(span.quant_type)
                    self.assertEqual(span.dtype_token, tensor.qtype.name)
                    self.assertEqual(span.shape, tensor.shape)
                else:
                    self.assertEqual(span.quant_type, int(tensor.qtype))
                    self.assertEqual((span.dtype_token, span.torch_dtype_name), ("U8", "uint8"))
                    self.assertEqual(span.shape, (*tensor.shape[:-1], self.fx.bytes_per_row(tensor.qtype, tensor.shape[-1])))
                    self.assertEqual(span.shape, tensor.raw.shape)

    def test_offsets_and_data_start_agree_with_the_gguf_reader(self) -> None:
        from gguf import GGUFReader

        header = gs.read_gguf_header(self.path)
        reference = {tensor.name: tensor for tensor in GGUFReader(self.path).tensors}
        self.assertEqual((header.version, header.alignment), (3, 32))
        self.assertEqual(header.data_start % 32, 0)
        for span in header.spans:
            with self.subTest(span.name):
                self.assertEqual(header.data_start + span.start, int(reference[span.name].data_offset))
                self.assertEqual(span.start % header.alignment, 0)
        # File order.
        self.assertEqual([span.start for span in header.spans], sorted(span.start for span in header.spans))

    def test_scalar_metadata_is_kept_and_arrays_are_skipped(self) -> None:
        metadata = gs.read_gguf_header(self.path).metadata
        self.assertEqual(metadata["general.architecture"], "flux")
        self.assertEqual(metadata["test.count"], 7)
        self.assertEqual(metadata["test.name"], "tiny")
        self.assertAlmostEqual(metadata["test.ratio"], 0.5)
        self.assertIs(metadata["test.flag"], True)
        self.assertNotIn("test.vocab", metadata)

    def test_a_custom_alignment_moves_the_data_start(self) -> None:
        path = self.fx.write_gguf(self.root / "aligned.gguf", self.tensors[:3], alignment=256)
        header = gs.read_gguf_header(path)
        self.assertEqual(header.alignment, 256)
        self.assertEqual(header.data_start % 256, 0)
        self.assertTrue(all(span.start % 256 == 0 for span in header.spans))

    def test_iq_types_parse_policy_free(self) -> None:
        import numpy as np
        from gguf import GGMLQuantizationType, GGUFWriter

        writer = GGUFWriter(self.root / "iq.gguf", "flux")
        rows = np.random.default_rng(0).integers(0, 256, size=(2, 2 * 136), dtype=np.uint8)
        writer.add_tensor("w", rows, raw_dtype=GGMLQuantizationType.IQ4_XS)
        writer.write_header_to_file()
        writer.write_kv_data_to_file()
        writer.write_tensors_to_file()
        writer.close()
        (span,) = gs.read_gguf_header(self.root / "iq.gguf").spans
        self.assertEqual(span.quant_type, int(GGMLQuantizationType.IQ4_XS))
        self.assertEqual(span.shape, (2, 272))
        self.assertEqual(gs.ggml_type_name(span.quant_type), "IQ4_XS")

    def test_the_magic_sniff(self) -> None:
        self.assertTrue(gs.is_gguf_file(self.path))
        other = self._file(b"\x08\0\0\0\0\0\0\0{}", "plain.safetensors")
        self.assertFalse(gs.is_gguf_file(other))
        with self.assertRaises(ValueError):
            gs.is_gguf_file(self.root / "missing.gguf")

    def test_unknown_type_ids_are_named_generically(self) -> None:
        self.assertEqual(gs.ggml_type_name(12), "Q4_K")
        self.assertEqual(gs.ggml_type_name(999), "type 999")


@unittest.skipUnless(_HAS_GGUF, "gguf is not installed")
class MalformedHeaderTests(_TempCase):
    def _refused(self, payload: bytes, *fragments: str, dtypes: dict[str, tuple[str, int]] | None = None) -> str:
        path = self._file(payload)
        with self.assertRaises(ValueError) as caught:
            gs.read_gguf_header(path, dtypes=dtypes if dtypes is not None else SAFETENSORS_DTYPES)
        message = str(caught.exception)
        self.assertIn(str(path), message)
        for fragment in fragments:
            self.assertIn(fragment, message)
        return message

    def test_a_valid_hand_built_file_parses(self) -> None:
        # The baseline every refusal below mutates: Q8_0 2x32 (ggml dims [32, 2]) + F32 4.
        payload = _raw_gguf(tensors=[("q", [32, 2], _Q8_0, 0), ("f", [4], _F32, 96)], data=b"\0" * 112)
        header = gs.read_gguf_header(self._file(payload))
        self.assertEqual([(s.name, s.shape, s.quant_type) for s in header.spans], [("q", (2, 34), _Q8_0), ("f", (4,), None)])

    def test_bad_magic_and_versions(self) -> None:
        self._refused(_raw_gguf(tensors=[], magic=b"GGUX"), "не является GGUF")
        self._refused(_raw_gguf(tensors=[], version=1), "версия GGUF 1")
        self._refused(_raw_gguf(tensors=[], version=4), "версия GGUF 4")

    def test_a_truncated_header(self) -> None:
        payload = _raw_gguf(tensors=[("q", [32, 2], _Q8_0, 0)])
        self._refused(payload[:30], "Обрезанный заголовок GGUF")

    def test_an_overflowing_string_length(self) -> None:
        kvs = struct.pack("<Q", 2**62) + b"key"
        self._refused(_raw_gguf(tensors=[], kvs=kvs, kv_count=1), "Некорректный заголовок GGUF")

    def test_misaligned_overlapping_and_past_eof_spans(self) -> None:
        self._refused(_raw_gguf(tensors=[("f", [4], _F32, 8)], data=b"\0" * 64), "«f»", "не кратно выравниванию 32")
        overlapping = _raw_gguf(tensors=[("a", [16], _F32, 0), ("b", [4], _F32, 32)], data=b"\0" * 64)
        self._refused(overlapping, "Перекрывающиеся тензоры", "«a»", "«b»")

    def test_spans_past_the_end_are_left_to_the_reader(self) -> None:
        # A header view: sizing works on a header-only file; `open_tensor_reader` refuses it (test_tensor_container).
        header = gs.read_gguf_header(self._file(_raw_gguf(tensors=[("f", [64], _F32, 0)], data=b"\0" * 16)))
        self.assertEqual(header.spans[0].nbytes, 256)

    def test_a_row_that_is_not_whole_blocks(self) -> None:
        self._refused(_raw_gguf(tensors=[("q", [48, 2], _Q4_K, 0)], data=b"\0" * 512), "«q»", "не кратна размеру блока 256")

    def test_an_unknown_type_id(self) -> None:
        self._refused(_raw_gguf(tensors=[("q", [32], 999, 0)], data=b"\0" * 64), "«q»", "неизвестный тип ggml 999")

    def test_a_plain_type_outside_the_table(self) -> None:
        payload = _raw_gguf(tensors=[("h", [4], _F16, 0)], data=b"\0" * 32)
        self._refused(payload, "«h»", "«F16»", dtypes={"F32": ("float32", 4), "U8": ("uint8", 1)})
        self._refused(_raw_gguf(tensors=[("q", [32], _Q8_0, 0)], data=b"\0" * 64), "«q»", "U8", dtypes={"F32": ("float32", 4)})

    def test_duplicate_tensor_names_and_keys(self) -> None:
        self._refused(_raw_gguf(tensors=[("f", [4], _F32, 0), ("f", [4], _F32, 32)], data=b"\0" * 64), "«f»", "повторяется")
        kvs = _kv_u32("a.b", 1) + _kv_u32("a.b", 2)
        self._refused(_raw_gguf(tensors=[], kvs=kvs, kv_count=2), "«a.b» повторяется")

    def test_a_bad_alignment(self) -> None:
        self._refused(_raw_gguf(tensors=[], kvs=_kv_u32("general.alignment", 24), kv_count=1), "выравнивание 24")
        wrong_type = _gguf_string("general.alignment") + struct.pack("<I", _STRING) + _gguf_string("32")
        self._refused(_raw_gguf(tensors=[], kvs=wrong_type, kv_count=1), "uint32")

    def test_dimension_counts(self) -> None:
        self._refused(_raw_gguf(tensors=[("f", [], _F32, 0)]), "«f»", "число осей 0")
        self._refused(_raw_gguf(tensors=[("f", [1] * 9, _F32, 0)], data=b"\0" * 32), "«f»", "число осей 9")

    def test_a_custom_alignment_is_honoured_for_the_data_start(self) -> None:
        payload = _raw_gguf(tensors=[("f", [4], _F32, 0)], kvs=_kv_u32("general.alignment", 64), kv_count=1, alignment=64, data=b"\0" * 16)
        header = gs.read_gguf_header(self._file(payload))
        self.assertEqual((header.alignment, header.data_start % 64), (64, 0))

    def test_arrays_of_strings_and_nested_arrays_are_skipped(self) -> None:
        strings = _gguf_string("tok.list") + struct.pack("<IIQ", _ARRAY, _STRING, 2) + _gguf_string("a") + _gguf_string("bc")
        nested = _gguf_string("tok.nested") + struct.pack("<IIQ", _ARRAY, _ARRAY, 1) + struct.pack("<IQ", _UINT32, 2) + struct.pack("<II", 5, 6)
        payload = _raw_gguf(tensors=[("f", [4], _F32, 0)], kvs=strings + nested + _kv_u32("x", 9), kv_count=3, data=b"\0" * 16)
        header = gs.read_gguf_header(self._file(payload))
        self.assertEqual(dict(header.metadata), {"x": 9})

    def test_zero_element_tensors_get_no_span_but_are_named(self) -> None:
        payload = _raw_gguf(tensors=[("marker", [0], _F32, 0), ("f", [4], _F32, 0)], data=b"\0" * 16)
        header = gs.read_gguf_header(self._file(payload))
        self.assertEqual([span.name for span in header.spans], ["f"])
        self.assertEqual(header.empty_tensors, ("marker",))


@unittest.skipUnless(_HAS_GGUF, "gguf is not installed")
class CountPreflightTests(_TempCase):
    """Hostile count fields are refused before their loop runs, in well under a second."""

    def _refused_fast(self, payload: bytes, *fragments: str) -> None:
        path = self._file(payload)
        started = time.monotonic()
        with self.assertRaises(ValueError) as caught:
            gs.read_gguf_header(path)
        elapsed = time.monotonic() - started
        self.assertLess(elapsed, 0.5, f"refusal took {elapsed:.3f}s")
        message = str(caught.exception)
        self.assertIn(str(path), message)
        for fragment in fragments:
            self.assertIn(fragment, message)

    @staticmethod
    def _counts(tensor_count: int, kv_count: int, tail: bytes = b"") -> bytes:
        return b"GGUF" + struct.pack("<IQQ", 3, tensor_count, kv_count) + tail

    @staticmethod
    def _array_kv(key: str, item_type: int, count: int, items: bytes = b"") -> bytes:
        return _gguf_string(key) + struct.pack("<IIQ", _ARRAY, item_type, count) + items

    def test_an_enormous_tensor_count(self) -> None:
        self._refused_fast(self._counts(2**63, 0, b"\0" * 64), "число тензоров", "больше предела")
        # Under the cap, but 1 M tensor infos cannot fit in a 64-byte tail.
        self._refused_fast(self._counts(1 << 20, 0, b"\0" * 64), "Обрезанный заголовок GGUF", "число тензоров")

    def test_an_enormous_kv_count(self) -> None:
        self._refused_fast(self._counts(0, 2**40, b"\0" * 64), "число ключей", "больше предела")
        self._refused_fast(self._counts(0, 60_000, b"\0" * 64), "Обрезанный заголовок GGUF", "число ключей")

    def test_an_enormous_string_array(self) -> None:
        huge = self._array_kv("tok.list", _STRING, 2**60, b"\0" * 64)
        self._refused_fast(self._counts(0, 1, huge), "длина массива «tok.list»", "больше предела")
        # Two million empty strings would need 16 MiB; the file holds 64 bytes.
        fitting_cap = self._array_kv("tok.list", _STRING, 2_000_000, b"\0" * 64)
        self._refused_fast(self._counts(0, 1, fitting_cap), "Обрезанный заголовок GGUF", "«tok.list»")

    def test_enormous_nested_arrays(self) -> None:
        huge = self._array_kv("tok.nested", _ARRAY, 2**50, b"\0" * 64)
        self._refused_fast(self._counts(0, 1, huge), "длина массива «tok.nested»", "больше предела")
        # Each level declares a count far larger than the bytes behind it.
        inner = struct.pack("<IQ", _ARRAY, 1_000_000) + b"\0" * 48
        self._refused_fast(self._counts(0, 1, self._array_kv("tok.nested", _ARRAY, 1, inner)), "Обрезанный заголовок GGUF", "«tok.nested»")

    def test_the_byte_ceiling_bounds_counts_on_a_sparse_file(self) -> None:
        # A file that claims to be huge (sparse, nothing written past the header):
        # with the cardinality cap lifted, the 100 MiB header ceiling — not the
        # file size — still refuses a count whose items cannot fit under it.
        path = self.root / "sparse.gguf"
        with path.open("wb") as handle:
            handle.write(self._counts(0, 1, self._array_kv("tok.list", _STRING, 20_000_000)))
            handle.truncate(gs.MAX_GGUF_HEADER_BYTES * 2)
        with patch.object(gs, "_MAX_ARRAY_ITEMS", 1 << 40):
            started = time.monotonic()
            with self.assertRaises(ValueError) as caught:
                gs.read_gguf_header(path)
        self.assertLess(time.monotonic() - started, 0.5)
        self.assertIn("заголовок длиннее", str(caught.exception))
        self.assertIn("«tok.list»", str(caught.exception))

    def test_the_whole_header_item_budget(self) -> None:
        # Two arrays each under the per-array cap whose sum passes a lowered total budget.
        kvs = self._array_kv("a", _STRING, 3, _gguf_string("") * 3) + self._array_kv("b", _STRING, 3, _gguf_string("") * 3)
        with patch.object(gs, "_MAX_HEADER_ITEMS", 6):
            self._refused_fast(self._counts(0, 2, kvs), "всего элементов заголовка", "«b»")

    def test_a_count_that_fits_the_bytes_but_not_the_cap(self) -> None:
        kvs = self._array_kv("a", _STRING, 3, _gguf_string("") * 3)
        with patch.object(gs, "_MAX_ARRAY_ITEMS", 2):
            self._refused_fast(self._counts(0, 1, kvs), "длина массива «a»: 3 больше предела 2")

    def test_a_real_sized_vocabulary_still_parses(self) -> None:
        # A Qwen3 header carries ~152 k-item string arrays; 200 k short strings must pass the caps quickly.
        items = _gguf_string("t") * 200_000
        payload = _raw_gguf(tensors=[("f", [4], _F32, 0)], kvs=self._array_kv("tokenizer.ggml.tokens", _STRING, 200_000, items), kv_count=1, data=b"\0" * 16)
        path = self._file(payload)
        started = time.monotonic()
        header = gs.read_gguf_header(path)
        self.assertLess(time.monotonic() - started, 1.0)
        self.assertEqual([span.name for span in header.spans], ["f"])


class MissingPackageTests(_TempCase):
    def test_a_missing_gguf_package_is_a_runtime_error_naming_it(self) -> None:
        path = self._file(_raw_gguf(tensors=[]))
        with patch.dict(sys.modules, {"gguf": None}):
            with self.assertRaises(RuntimeError) as caught:
                gs.read_gguf_header(path)
        self.assertIn("gguf", str(caught.exception))


if __name__ == "__main__":
    unittest.main()
