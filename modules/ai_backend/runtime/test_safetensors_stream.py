"""
File: modules/ai_backend/runtime/test_safetensors_stream.py

Purpose:
Unit tests for `runtime/safetensors_stream.py`: the header parser's ceiling and
truncation refusals, the per-tensor contract under the default and a custom
dtype table, file order and the overlap refusal, the reader's end-of-file check,
`ByteProgress` (rate limit on an injected clock, the 0.2 s interval, final
report without a duplicate frame, caller-owned wording and logger), chunked
tensor reads (bounded `readinto`, per-chunk byte reports), the
opt-in 8-bit float table (bit-exact fp8 round trip) and
`fill_module_from_safetensors` (1:1 key map + cast, 1:N convert with and
without header-declared keys, per-key `targets` incl. leaving keys on `meta`,
the `write` seam, per-tensor progress, the one-tensor host peak, argument
validation, and every refusal), plus the filler on a GGUF file
(`GgufFillTests`: quantized uint8 rows and plain tensors, 1:1 and 1:N with
declared keys, file order and one source tensor alive at a time).

Notes:
Torch-free except `ReadTensorTests`, `F8RoundTripTests`, `FillModuleTests` and
`GgufFillTests`, which need real torch on the CPU and skip themselves without
it (`GgufFillTests` also needs the `gguf` package). Every file is written into a
temporary directory removed by the test.
"""

from __future__ import annotations

import importlib.util
import json
import logging
import struct
import tempfile
import unittest
import weakref
from pathlib import Path
from typing import Any
from unittest.mock import patch

from . import safetensors_stream as ss

#: The default table plus one 8-bit float token, as a caller that reads fp8 would pass it.
_WITH_F8 = {**ss.SAFETENSORS_DTYPES, "F8_E4M3": ("float8_e4m3fn", 1)}


def _write_raw(path: Path, header: dict[str, object], data: bytes = b"") -> None:
    """Write a safetensors container: length prefix, JSON header, then `data`."""
    payload = json.dumps(header).encode("utf-8")
    path.write_bytes(struct.pack("<Q", len(payload)) + payload + data)


def _entry(dtype: str, shape: list[int], start: int, end: int) -> dict[str, object]:
    """One header entry."""
    return {"dtype": dtype, "shape": shape, "data_offsets": [start, end]}


class _TempCase(unittest.TestCase):
    """A `TestCase` with a temp directory that is removed afterwards."""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)


class HeaderParserTests(_TempCase):
    def test_returns_the_header_and_the_absolute_data_start(self) -> None:
        path = self.root / "ok.safetensors"
        header = {"__metadata__": {"format": "pt"}, "w": _entry("F32", [2], 0, 8)}
        _write_raw(path, header, b"\0" * 8)
        parsed, data_start = ss.read_safetensors_header_and_data_start(path)
        self.assertEqual(parsed, header)
        self.assertEqual(data_start, 8 + len(json.dumps(header).encode("utf-8")))
        self.assertEqual(ss.read_safetensors_header(path), header)

    def test_a_header_above_the_ceiling_is_refused_before_it_is_read(self) -> None:
        path = self.root / "huge.safetensors"
        path.write_bytes(struct.pack("<Q", ss.MAX_SAFETENSORS_HEADER_BYTES + 1) + b"{}")
        with self.assertRaises(ValueError) as caught:
            ss.read_safetensors_header(path)
        self.assertIn("Некорректный заголовок safetensors", str(caught.exception))

    def test_a_zero_length_header_is_refused(self) -> None:
        path = self.root / "zero.safetensors"
        path.write_bytes(struct.pack("<Q", 0))
        with self.assertRaises(ValueError):
            ss.read_safetensors_header(path)

    def test_a_truncated_header_is_refused(self) -> None:
        path = self.root / "cut.safetensors"
        path.write_bytes(struct.pack("<Q", 100) + b'{"a": 1')
        with self.assertRaises(ValueError) as caught:
            ss.read_safetensors_header(path)
        self.assertIn("Обрезанный заголовок", str(caught.exception))

    def test_a_file_shorter_than_the_length_prefix_is_refused(self) -> None:
        path = self.root / "tiny.safetensors"
        path.write_bytes(b"\x01\x02")
        with self.assertRaises(ValueError) as caught:
            ss.read_safetensors_header(path)
        self.assertIn("слишком мал", str(caught.exception))

    def test_a_non_json_or_non_object_header_is_refused(self) -> None:
        for name, payload in (("bad.safetensors", b"\xff\xfe"), ("list.safetensors", b"[1, 2]")):
            with self.subTest(name=name):
                path = self.root / name
                path.write_bytes(struct.pack("<Q", len(payload)) + payload)
                with self.assertRaises(ValueError):
                    ss.read_safetensors_header(path)

    def test_a_missing_file_is_a_value_error(self) -> None:
        with self.assertRaises(ValueError):
            ss.read_safetensors_header(self.root / "absent.safetensors")


class ParseTensorSpansTests(unittest.TestCase):
    SOURCE = Path("/fictional/model.safetensors")

    def test_the_default_table_has_no_8_bit_floats(self) -> None:
        self.assertFalse([token for token in ss.SAFETENSORS_DTYPES if token.startswith("F8")])

    def test_the_default_table_refuses_an_fp8_tensor_and_lists_what_it_knows(self) -> None:
        header = {"w": _entry("F8_E4M3", [2], 0, 2)}
        with self.assertRaises(ValueError) as caught:
            ss.parse_tensor_spans(header, source=self.SOURCE)
        message = str(caught.exception)
        self.assertIn("неподдерживаемый тип данных «F8_E4M3»", message)
        self.assertIn("BF16", message)

    def test_a_custom_table_maps_its_own_tokens(self) -> None:
        header = {"w": _entry("F8_E4M3", [2, 3], 0, 6)}
        (span,) = ss.parse_tensor_spans(header, source=self.SOURCE, dtypes=_WITH_F8)
        self.assertEqual(span.torch_dtype_name, "float8_e4m3fn")
        self.assertEqual(span.shape, (2, 3))
        self.assertEqual(span.nbytes, 6)

    def test_a_custom_table_is_also_what_refuses(self) -> None:
        header = {"w": _entry("F32", [1], 0, 4)}
        with self.assertRaises(ValueError) as caught:
            ss.parse_tensor_spans(header, source=self.SOURCE, dtypes={"BF16": ("bfloat16", 2)})
        self.assertIn("(известные: BF16)", str(caught.exception))

    def test_spans_come_back_in_file_order_and_metadata_is_skipped(self) -> None:
        header = {
            "__metadata__": {"format": "pt"},
            "a": _entry("F32", [1], 8, 12),
            "b": _entry("F32", [2], 0, 8),
        }
        spans = ss.parse_tensor_spans(header, source=self.SOURCE)
        self.assertEqual([span.name for span in spans], ["b", "a"])

    def test_overlapping_spans_are_refused_naming_both_keys(self) -> None:
        header = {"a": _entry("F32", [2], 0, 8), "b": _entry("F32", [2], 4, 12)}
        with self.assertRaises(ValueError) as caught:
            ss.parse_tensor_spans(header, source=self.SOURCE)
        self.assertIn("«a»", str(caught.exception))
        self.assertIn("«b»", str(caught.exception))

    def test_a_length_that_disagrees_with_the_shape_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            ss.parse_tensor_spans({"w": _entry("F32", [3], 0, 8)}, source=self.SOURCE)


class ReaderConstructionTests(_TempCase):
    def test_the_reader_forwards_its_dtype_table(self) -> None:
        path = self.root / "fp8.safetensors"
        _write_raw(path, {"w": _entry("F8_E4M3", [4], 0, 4)}, b"\x01\x02\x03\x04")
        with self.assertRaises(ValueError):
            ss.StreamingSafetensorsReader(path)
        reader = ss.StreamingSafetensorsReader(path, dtypes=_WITH_F8)
        self.assertEqual(reader.keys_in_file_order(), ["w"])
        self.assertEqual(reader.total_tensor_bytes, 4)

    def test_a_span_past_the_end_of_the_file_is_refused(self) -> None:
        path = self.root / "short.safetensors"
        _write_raw(path, {"w": _entry("F32", [2], 0, 8)}, b"\0" * 4)
        with self.assertRaises(ValueError) as caught:
            ss.StreamingSafetensorsReader(path)
        self.assertIn("Обрезанный safetensors", str(caught.exception))


@unittest.skipUnless(importlib.util.find_spec("torch") is not None, "torch is not installed")
class ReadTensorTests(_TempCase):
    def test_read_tensor_returns_the_bytes_in_file_order(self) -> None:
        import torch

        first = struct.pack("<2f", 1.5, -2.0)
        second = struct.pack("<f", 3.25)
        path = self.root / "real.safetensors"
        _write_raw(
            path,
            {"z": _entry("F32", [2], 0, 8), "a": _entry("F32", [1], 8, 12)},
            first + second,
        )
        reader = ss.StreamingSafetensorsReader(path)
        with reader:
            values = [reader.read_tensor(span) for span in reader.spans]
        self.assertEqual([span.name for span in reader.spans], ["z", "a"])
        self.assertTrue(torch.equal(values[0], torch.tensor([1.5, -2.0])))
        self.assertTrue(torch.equal(values[1], torch.tensor([3.25])))

    def test_read_tensor_reads_a_big_tensor_in_bounded_chunks(self) -> None:
        import torch

        values = [float(v) for v in range(25)]
        path = self.root / "big.safetensors"
        _write_raw(path, {"w": _entry("F32", [25], 0, 100)}, _f32_bytes(values))
        reader = ss.StreamingSafetensorsReader(path)
        chunks: list[int] = []
        with reader:
            (span,) = reader.spans
            value = reader.read_tensor(span, on_bytes=chunks.append, chunk_bytes=32)
        # Every readinto is bounded by the chunk; together they are the whole span.
        self.assertGreaterEqual(len(chunks), 4)
        self.assertTrue(all(0 < chunk <= 32 for chunk in chunks))
        self.assertEqual(sum(chunks), 100)
        self.assertTrue(torch.equal(value, torch.tensor(values)))

    def test_read_tensor_refuses_a_non_positive_chunk(self) -> None:
        path = self.root / "real.safetensors"
        _write_raw(path, {"w": _entry("F32", [1], 0, 4)}, b"\0" * 4)
        reader = ss.StreamingSafetensorsReader(path)
        with reader, self.assertRaises(ValueError):
            reader.read_tensor(reader.spans[0], chunk_bytes=0)

    def test_read_tensor_outside_the_context_manager_is_refused(self) -> None:
        path = self.root / "real.safetensors"
        _write_raw(path, {"w": _entry("F32", [1], 0, 4)}, b"\0" * 4)
        reader = ss.StreamingSafetensorsReader(path)
        with self.assertRaises(RuntimeError):
            reader.read_tensor(reader.spans[0])


class ByteProgressTests(unittest.TestCase):
    def setUp(self) -> None:
        self.now = 10.0
        self.seen: list[tuple[int, int, str]] = []
        self.logger = logging.getLogger("test.runtime.safetensors_stream.owner")

    def _progress(self, done: int, total: int, key: str) -> None:
        self.seen.append((done, total, key))

    def _reporter(self, callback: ss.ByteProgressCb | None, **kwargs: float) -> ss.ByteProgress:
        return ss.ByteProgress(
            callback, 30, owner="Some model", logger=self.logger, clock=lambda: self.now, **kwargs
        )

    def test_reports_are_rate_limited_and_the_last_one_always_fires(self) -> None:
        reporter = self._reporter(self._progress, interval_seconds=2.0)
        reporter.advance(10, "a")
        self.now += 1.0
        reporter.advance(10, "b")
        self.now += 2.0
        reporter.advance(5, "c")
        reporter.advance(5, "d", final=True)
        self.assertEqual(self.seen, [(25, 30, "c"), (30, 30, "d")])
        self.assertEqual(reporter.done_bytes, 30)

    def test_a_raising_callback_is_logged_with_the_owner_on_the_given_logger(self) -> None:
        def explode(done: int, total: int, key: str) -> None:
            raise RuntimeError("peer is gone")

        reporter = self._reporter(explode, interval_seconds=0.0)
        with self.assertLogs(self.logger, level=logging.DEBUG) as captured:
            reporter.advance(30, "a", final=True)
        self.assertEqual(
            [record.getMessage() for record in captured.records],
            ["Some model: обработчик прогресса загрузки бросил исключение: peer is gone"],
        )
        self.assertEqual(captured.records[0].name, self.logger.name)
        self.assertEqual(reporter.done_bytes, 30)

    def test_the_interval_is_five_frames_per_second(self) -> None:
        self.assertEqual(ss.PROGRESS_MIN_INTERVAL_SECONDS, 0.2)

    def test_the_interval_is_honoured_on_the_injected_clock(self) -> None:
        # Binary-exact steps, so the float clock hits the interval exactly.
        reporter = self._reporter(self._progress, interval_seconds=0.25)
        for key in "abcdef":
            # One 1 B chunk every 0.125 s: a report on every second one.
            self.now += 0.125
            reporter.advance(1, key)
        self.assertEqual(self.seen, [(2, 30, "b"), (4, 30, "d"), (6, 30, "f")])

    def test_a_final_with_nothing_new_since_the_last_report_is_not_repeated(self) -> None:
        reporter = self._reporter(self._progress, interval_seconds=0.0)
        reporter.advance(30, "a")
        reporter.advance(0, "a", final=True)
        self.assertEqual(self.seen, [(30, 30, "a")])
        # A final after bytes the throttle swallowed still reports them.
        self.seen.clear()
        throttled = self._reporter(self._progress)
        throttled.advance(30, "b")
        throttled.advance(0, "b", final=True)
        self.assertEqual(self.seen, [(30, 30, "b")])

    def test_no_callback_still_counts(self) -> None:
        reporter = self._reporter(None)
        reporter.advance(4, "a", final=True)
        self.assertEqual(reporter.done_bytes, 4)


class F8TableTests(unittest.TestCase):
    def test_the_opt_in_table_is_the_default_plus_the_two_8_bit_floats(self) -> None:
        extra = set(ss.SAFETENSORS_DTYPES_WITH_F8) - set(ss.SAFETENSORS_DTYPES)
        self.assertEqual(extra, {"F8_E4M3", "F8_E5M2"})
        for token, mapped in ss.SAFETENSORS_DTYPES.items():
            self.assertEqual(ss.SAFETENSORS_DTYPES_WITH_F8[token], mapped)
        self.assertEqual(ss.SAFETENSORS_DTYPES_WITH_F8["F8_E5M2"], ("float8_e5m2", 1))


_HAS_TORCH = importlib.util.find_spec("torch") is not None


@unittest.skipUnless(_HAS_TORCH, "torch is not installed")
class F8RoundTripTests(_TempCase):
    def test_fp8_tensors_read_back_bit_exact_with_the_opt_in_table(self) -> None:
        import torch

        for token, dtype in (("F8_E4M3", torch.float8_e4m3fn), ("F8_E5M2", torch.float8_e5m2)):
            with self.subTest(token=token):
                original = torch.tensor([0.5, -1.0, 2.0, 448.0 if token == "F8_E4M3" else 57344.0]).to(dtype)
                payload = original.view(torch.uint8).numpy().tobytes()
                path = self.root / f"{token}.safetensors"
                _write_raw(path, {"w": _entry(token, [2, 2], 0, 4)}, payload)
                reader = ss.StreamingSafetensorsReader(path, dtypes=ss.SAFETENSORS_DTYPES_WITH_F8)
                with reader:
                    (value,) = [reader.read_tensor(span) for span in reader.spans]
                self.assertEqual(value.dtype, dtype)
                self.assertEqual(tuple(value.shape), (2, 2))
                self.assertTrue(torch.equal(value.view(torch.uint8).flatten(), original.view(torch.uint8)))


def _f32_bytes(values: list[float]) -> bytes:
    """Little-endian float32 bytes of `values`."""
    return struct.pack(f"<{len(values)}f", *values)


def _write_f32_file(path: Path, tensors: dict[str, tuple[list[int], list[float]]]) -> None:
    """Write a float32 safetensors file of `{key: (shape, values)}`, back to back in dict order."""
    header: dict[str, object] = {}
    data = b""
    for key, (shape, values) in tensors.items():
        chunk = _f32_bytes(values)
        header[key] = _entry("F32", shape, len(data), len(data) + len(chunk))
        data += chunk
    _write_raw(path, header, data)


@unittest.skipUnless(_HAS_TORCH, "torch is not installed")
class FillModuleTests(_TempCase):
    """`fill_module_from_safetensors` on a tiny CPU module built on `meta`."""

    def setUp(self) -> None:
        super().setUp()
        import torch

        class Tiny(torch.nn.Module):
            def __init__(self) -> None:
                super().__init__()
                self.proj = torch.nn.Linear(2, 3)
                self.register_buffer("stat", torch.zeros(2))
                self.register_buffer("table", torch.zeros(4), persistent=False)

        with torch.device("meta"):
            self.module = Tiny()
        # The non-persistent buffer is the caller's to make real before the fill.
        self.module.table = torch.arange(4.0)
        self.logger = logging.getLogger("test.runtime.safetensors_stream.fill")
        self.weight = [float(v) for v in range(6)]
        self.bias = [10.0, 11.0, 12.0]
        self.stat = [7.0, 8.0]

    def _fill(self, files: list[Path], **kwargs: object) -> ss.FillReport:
        options: dict[str, object] = {
            "device": "cpu",
            "key_map": lambda key: key,
            "cast": lambda key, tensor: tensor,
            "owner": "Tiny",
            "logger": self.logger,
        }
        options.update(kwargs)
        return ss.fill_module_from_safetensors(self.module, files, **options)

    def _complete_file(self, name: str = "all.safetensors") -> Path:
        path = self.root / name
        _write_f32_file(
            path,
            {"proj.weight": ([3, 2], self.weight), "proj.bias": ([3], self.bias), "stat": ([2], self.stat)},
        )
        return path

    def test_fills_parameters_and_buffers_across_files(self) -> None:
        import torch

        first = self.root / "a.safetensors"
        second = self.root / "b.safetensors"
        _write_f32_file(first, {"proj.weight": ([3, 2], self.weight)})
        _write_f32_file(second, {"proj.bias": ([3], self.bias), "stat": ([2], self.stat)})
        seen: list[tuple[int, int, str]] = []
        report = self._fill([first, second], progress=lambda d, t, k: seen.append((d, t, k)))
        self.assertEqual(report, ss.FillReport(loaded=3, skipped=0, bytes_read=44))
        self.assertIsInstance(self.module.proj.weight, torch.nn.Parameter)
        self.assertFalse(self.module.proj.weight.requires_grad)
        self.assertTrue(torch.equal(self.module.proj.weight, torch.tensor(self.weight).reshape(3, 2)))
        self.assertTrue(torch.equal(self.module.proj.bias, torch.tensor(self.bias)))
        self.assertIn("stat", dict(self.module.named_buffers()))
        self.assertNotIsInstance(self.module.stat, torch.nn.Parameter)
        self.assertTrue(torch.equal(self.module.stat, torch.tensor(self.stat)))
        # The non-persistent buffer stays out of the state dict and untouched.
        self.assertNotIn("table", self.module.state_dict())
        self.assertTrue(torch.equal(self.module.table, torch.arange(4.0)))
        self.assertEqual(seen[-1], (44, 44, "stat"))

    def test_key_map_renames_and_skips_without_reading(self) -> None:
        path = self.root / "prefixed.safetensors"
        _write_f32_file(
            path,
            {
                "model.proj.weight": ([3, 2], self.weight),
                "model.proj.bias": ([3], self.bias),
                "model.stat": ([2], self.stat),
                "visual.extra": ([5], [0.0] * 5),
            },
        )

        def key_map(key: str) -> str | None:
            return None if key.startswith("visual.") else key.removeprefix("model.")

        report = self._fill([path], key_map=key_map)
        self.assertEqual(report, ss.FillReport(loaded=3, skipped=1, bytes_read=44))

    def test_cast_runs_before_the_move_and_decides_the_dtype(self) -> None:
        import torch

        casts: list[str] = []

        def cast(key: str, tensor: torch.Tensor) -> torch.Tensor:
            casts.append(key)
            self.assertEqual(tensor.device.type, "cpu")
            return tensor.to(torch.bfloat16) if key.startswith("proj.") else tensor

        self._fill([self._complete_file()], cast=cast)
        self.assertEqual(sorted(casts), ["proj.bias", "proj.weight", "stat"])
        self.assertEqual(self.module.proj.weight.dtype, torch.bfloat16)
        self.assertEqual(self.module.stat.dtype, torch.float32)

    def test_an_unexpected_key_is_refused_before_any_read(self) -> None:
        path = self.root / "extra.safetensors"
        _write_f32_file(
            path,
            {
                "proj.weight": ([3, 2], self.weight),
                "proj.bias": ([3], self.bias),
                "stat": ([2], self.stat),
                "ghost": ([1], [0.0]),
            },
        )
        casts: list[str] = []
        with self.assertRaises(ValueError) as caught:
            self._fill([path], cast=lambda key, tensor: casts.append(key) or tensor)
        self.assertIn("extra.safetensors:ghost", str(caught.exception))
        self.assertEqual(casts, [])
        self.assertTrue(self.module.proj.weight.is_meta)

    def test_a_key_provided_by_two_files_is_refused_naming_both(self) -> None:
        first = self._complete_file("first.safetensors")
        second = self.root / "second.safetensors"
        _write_f32_file(second, {"stat": ([2], self.stat)})
        with self.assertRaises(ValueError) as caught:
            self._fill([first, second])
        message = str(caught.exception)
        self.assertIn("second.safetensors:stat", message)
        self.assertIn("first.safetensors", message)

    def test_two_file_keys_mapping_to_one_state_key_are_a_duplicate(self) -> None:
        path = self.root / "aliases.safetensors"
        _write_f32_file(
            path,
            {
                "proj.weight": ([3, 2], self.weight),
                "proj.bias": ([3], self.bias),
                "stat": ([2], self.stat),
                "stat_alias": ([2], self.stat),
            },
        )
        with self.assertRaises(ValueError) as caught:
            self._fill([path], key_map=lambda key: key.removesuffix("_alias"))
        self.assertIn("aliases.safetensors:stat_alias", str(caught.exception))

    def test_a_missing_expected_key_is_refused(self) -> None:
        path = self.root / "partial.safetensors"
        _write_f32_file(path, {"proj.weight": ([3, 2], self.weight), "proj.bias": ([3], self.bias)})
        with self.assertRaises(ValueError) as caught:
            self._fill([path])
        message = str(caught.exception)
        self.assertIn("partial.safetensors", message)
        self.assertIn("1 шт., первые: stat", message)

    def test_a_shape_mismatch_is_refused_naming_file_and_key(self) -> None:
        path = self.root / "shape.safetensors"
        _write_f32_file(
            path,
            {"proj.weight": ([2, 3], self.weight), "proj.bias": ([3], self.bias), "stat": ([2], self.stat)},
        )
        with self.assertRaises(ValueError) as caught:
            self._fill([path])
        message = str(caught.exception)
        self.assertIn("«proj.weight»", message)
        self.assertIn("shape.safetensors", message)
        self.assertIn("(2, 3)", message)

    def test_a_meta_tensor_left_after_the_fill_is_refused(self) -> None:
        import torch

        with torch.device("meta"):
            self.module.table = torch.zeros(4)
        with self.assertRaises(ValueError) as caught:
            self._fill([self._complete_file()])
        self.assertIn("table", str(caught.exception))

    def test_the_default_table_still_refuses_fp8_in_a_fill(self) -> None:
        path = self.root / "fp8.safetensors"
        _write_raw(path, {"stat": _entry("F8_E4M3", [2], 0, 2)}, b"\x01\x02")
        with self.assertRaises(ValueError) as caught:
            self._fill([path])
        self.assertIn("F8_E4M3", str(caught.exception))


@unittest.skipUnless(_HAS_TORCH, "torch is not installed")
class FillModuleConvertTests(FillModuleTests):
    """The 1:N `convert`, `expected_keys_of`, `targets` and `write` forms of the filler.

    Inherits `FillModuleTests` for its fixture, so every 1:1 case above also
    runs once more here unchanged.
    """

    def _packed_file(self, name: str = "packed.safetensors", **extra: tuple[list[int], list[float]]) -> Path:
        """`proj.packed` [3, 3] = weight columns 0-1 + bias column 2, then `stat`."""
        packed = [value for row in range(3) for value in (*self.weight[row * 2 : row * 2 + 2], self.bias[row])]
        path = self.root / name
        _write_f32_file(path, {"proj.packed": ([3, 3], packed), "stat": ([2], self.stat), **extra})
        return path

    @staticmethod
    def _split(key: str, tensor: object) -> list[tuple[str, object]]:
        """The 1:N conversion of `_packed_file`: one packed tensor -> weight + bias."""
        if key == "proj.packed":
            return [("proj.weight", tensor[:, :2]), ("proj.bias", tensor[:, 2])]
        return [(key, tensor)]

    @staticmethod
    def _declared(key: str) -> list[str]:
        return ["proj.weight", "proj.bias"] if key == "proj.packed" else [key]

    def _convert_fill(self, files: list[Path], **kwargs: object) -> ss.FillReport:
        options: dict[str, object] = {"device": "cpu", "convert": self._split, "owner": "Tiny", "logger": self.logger}
        options.update(kwargs)
        return ss.fill_module_from_safetensors(self.module, files, **options)

    def test_one_file_tensor_may_become_several_state_keys(self) -> None:
        import torch

        for declared in (None, self._declared):
            with self.subTest(expected_keys_of=declared is not None):
                self.setUp()
                report = self._convert_fill([self._packed_file()], expected_keys_of=declared)
                self.assertEqual(report, ss.FillReport(loaded=3, skipped=0, bytes_read=44))
                self.assertTrue(torch.equal(self.module.proj.weight, torch.tensor(self.weight).reshape(3, 2)))
                self.assertTrue(torch.equal(self.module.proj.bias, torch.tensor(self.bias)))
                self.assertFalse(self.module.proj.weight.requires_grad)
                self.assertTrue(torch.equal(self.module.stat, torch.tensor(self.stat)))

    def test_declared_keys_refuse_a_wrong_checkpoint_before_any_read(self) -> None:
        calls: list[str] = []

        def convert(key: str, tensor: object) -> list[tuple[str, object]]:
            calls.append(key)
            return self._split(key, tensor)

        path = self._packed_file("ghost.safetensors", ghost=([1], [0.0]))
        with self.assertRaises(ValueError) as caught:
            self._convert_fill([path], convert=convert, expected_keys_of=self._declared)
        self.assertIn("ghost.safetensors:ghost -> ghost", str(caught.exception))
        path = self.root / "partial.safetensors"
        _write_f32_file(path, {"stat": ([2], self.stat)})
        with self.assertRaises(ss.MissingWeightsError) as missing:
            self._convert_fill([path], convert=convert, expected_keys_of=self._declared)
        self.assertEqual(missing.exception.missing, ("proj.bias", "proj.weight"))
        self.assertEqual(calls, [])

    def test_a_file_key_declaring_no_state_key_is_skipped_unread(self) -> None:
        calls: list[str] = []

        def convert(key: str, tensor: object) -> list[tuple[str, object]]:
            calls.append(key)
            return self._split(key, tensor)

        path = self._packed_file(extra=([5], [0.0] * 5))
        report = self._convert_fill(
            [path], convert=convert, expected_keys_of=lambda key: [] if key == "extra" else self._declared(key)
        )
        self.assertEqual(report, ss.FillReport(loaded=3, skipped=1, bytes_read=44))
        self.assertEqual(calls, ["proj.packed", "stat"])

    def test_convert_must_produce_exactly_the_declared_keys(self) -> None:
        with self.assertRaises(ValueError) as caught:
            self._convert_fill(
                [self._packed_file()],
                convert=lambda key, tensor: self._split(key, tensor)[:1],
                expected_keys_of=self._declared,
            )
        self.assertIn("«proj.packed»", str(caught.exception))
        self.assertIn("proj.bias", str(caught.exception))

    def test_undeclared_keys_are_checked_per_tensor(self) -> None:
        with self.assertRaises(ValueError) as unexpected:
            self._convert_fill([self._packed_file()], convert=lambda key, tensor: [(f"x.{key}", tensor)])
        self.assertIn("packed.safetensors:proj.packed -> x.proj.packed", str(unexpected.exception))
        self.setUp()
        with self.assertRaises(ValueError) as duplicate:
            self._convert_fill(
                [self._packed_file()],
                convert=lambda key, tensor: self._split(key, tensor) + [("proj.bias", tensor[:, 2])],
            )
        self.assertIn("proj.bias (уже из packed.safetensors)", str(duplicate.exception))

    def test_without_declared_keys_a_missing_key_is_refused_after_the_reads(self) -> None:
        reads: list[str] = []
        original = ss.StreamingSafetensorsReader.read_tensor

        def read(reader: ss.StreamingSafetensorsReader, span: ss.TensorSpan, **kwargs: Any) -> object:
            reads.append(span.name)
            return original(reader, span, **kwargs)

        path = self.root / "partial.safetensors"
        _write_f32_file(path, {"stat": ([2], self.stat)})
        with patch.object(ss.StreamingSafetensorsReader, "read_tensor", read):
            with self.assertRaises(ss.MissingWeightsError) as caught:
                self._convert_fill([path])
        self.assertEqual(reads, ["stat"])
        self.assertEqual(caught.exception.missing, ("proj.bias", "proj.weight"))
        self.assertIn("partial.safetensors", str(caught.exception))

    def test_targets_can_leave_keys_on_meta(self) -> None:
        import torch

        on_meta = lambda key: ss.LEAVE_ON_META if key == "proj.bias" else "cpu"  # noqa: E731
        # 1:1 and declared: the bias tensor is not even read.
        report = self._fill([self._complete_file()], targets=on_meta)
        self.assertEqual(report, ss.FillReport(loaded=2, skipped=0, bytes_read=32, left_on_meta=1))
        self.assertTrue(self.module.proj.bias.is_meta)
        self.assertTrue(torch.equal(self.module.proj.weight, torch.tensor(self.weight).reshape(3, 2)))
        # 1:N where one file tensor yields a kept and a meta key: read, half kept.
        self.setUp()
        report = self._convert_fill([self._packed_file()], targets=on_meta)
        self.assertEqual(report, ss.FillReport(loaded=2, skipped=0, bytes_read=44, left_on_meta=1))
        self.assertTrue(self.module.proj.bias.is_meta)
        self.assertFalse(self.module.proj.weight.is_meta)

    def test_nothing_read_reports_once_and_still_fills_nothing(self) -> None:
        seen: list[tuple[int, int, str]] = []
        report = self._fill(
            [self._complete_file()],
            targets=lambda key: ss.LEAVE_ON_META,
            progress=lambda d, t, k: seen.append((d, t, k)),
        )
        self.assertEqual(report, ss.FillReport(loaded=0, skipped=0, bytes_read=0, left_on_meta=3))
        self.assertEqual(seen, [(0, 0, "")])

    def test_write_replaces_the_shape_check_and_placement_per_file_tensor(self) -> None:
        calls: list[list[tuple[str, tuple[int, ...], str]]] = []

        def write(writes: list[ss.TensorWrite]) -> None:
            calls.append([(key, tuple(value.shape), str(place)) for key, value, place in writes])
            for key, value, _ in writes:
                ss._set_module_tensor(self.module, key, value.reshape(-1).clone())

        self._convert_fill([self._packed_file()], write=write)
        self.assertEqual(
            calls,
            [[("proj.weight", (3, 2), "cpu"), ("proj.bias", (3,), "cpu")], [("stat", (2,), "cpu")]],
        )
        # The filler did not refuse the flattened weight: the writer owns the shape.
        self.assertEqual(tuple(self.module.proj.weight.shape), (6,))

    def test_one_report_per_tensor_and_the_last_one_is_final(self) -> None:
        seen: list[tuple[int, int, str]] = []
        self._fill(
            [self._complete_file()],
            progress=lambda d, t, k: seen.append((d, t, k)),
            progress_interval_seconds=0.0,
        )
        self.assertEqual(seen, [(24, 44, "proj.weight"), (36, 44, "proj.bias"), (44, 44, "stat")])

    def test_a_big_tensor_reports_per_chunk_and_ends_once_at_the_file_bytes(self) -> None:
        import torch

        seen: list[tuple[int, int, str]] = []
        path = self._complete_file()
        with patch.object(ss, "READ_CHUNK_BYTES", 8):
            report = self._fill([path], progress=lambda d, t, k: seen.append((d, t, k)), progress_interval_seconds=0.0)
        # proj.weight (24 B) in three 8 B chunks, proj.bias (12 B) in 8 + 4, stat (8 B) in one.
        self.assertEqual(
            seen,
            [
                (8, 44, "proj.weight"), (16, 44, "proj.weight"), (24, 44, "proj.weight"),
                (32, 44, "proj.bias"), (36, 44, "proj.bias"), (44, 44, "stat"),
            ],
        )
        self.assertEqual(report.bytes_read, 44)
        self.assertTrue(torch.equal(self.module.proj.weight, torch.tensor(self.weight).reshape(3, 2)))

    def test_the_final_report_comes_after_the_last_write_even_when_throttled(self) -> None:
        seen: list[tuple[int, int, str]] = []
        written: list[str] = []

        def write(writes: list[ss.TensorWrite]) -> None:
            for key, value, _ in writes:
                written.append(key)
                ss._set_module_tensor(self.module, key, value.clone())

        def progress(done: int, total: int, key: str) -> None:
            seen.append((done, total, key))
            self.assertEqual(len(written), 3)

        # A huge interval: only the guaranteed final frame fires, after every write.
        self._fill([self._complete_file()], write=write, progress=progress, progress_interval_seconds=3600.0)
        self.assertEqual(seen, [(44, 44, "stat")])

    def test_no_earlier_source_tensor_is_alive_at_the_next_read(self) -> None:
        import torch

        live: list[int] = []
        sources: list[weakref.ref] = []
        original = ss.StreamingSafetensorsReader.read_tensor

        def read(reader: ss.StreamingSafetensorsReader, span: ss.TensorSpan, **kwargs: Any) -> object:
            live.append(sum(1 for ref in sources if ref() is not None))
            tensor = original(reader, span, **kwargs)
            sources.append(weakref.ref(tensor))
            return tensor

        def convert(key: str, tensor: torch.Tensor) -> list[tuple[str, torch.Tensor]]:
            # bf16 copies: the module must not own the source bytes.
            return [(state_key, value.to(torch.bfloat16)) for state_key, value in self._split(key, tensor)]

        with patch.object(ss.StreamingSafetensorsReader, "read_tensor", read):
            self._convert_fill([self._packed_file()], convert=convert)
        live.append(sum(1 for ref in sources if ref() is not None))
        self.assertEqual(live, [0, 0, 0])

    def test_log_summary_false_leaves_the_records_to_the_caller(self) -> None:
        with self.assertNoLogs(self.logger, level="INFO"):
            self._fill([self._complete_file()], log_summary=False)
        with self.assertLogs(self.logger, level="INFO") as captured:
            self._fill([self._complete_file()])
        self.assertEqual(len(captured.records), 2)

    def test_conversion_arguments_must_name_one_form(self) -> None:
        cases = {
            "neither": {"key_map": None},
            "both": {"convert": self._split},
            "cast with convert": {"key_map": None, "convert": self._split},
            "declared without convert": {"expected_keys_of": self._declared},
        }
        for label, kwargs in cases.items():
            with self.subTest(label):
                with self.assertRaises(TypeError):
                    self._fill([self._complete_file()], **kwargs)


_HAS_GGUF = importlib.util.find_spec("gguf") is not None


@unittest.skipUnless(_HAS_TORCH and _HAS_GGUF, "torch or gguf is not installed")
class GgufFillTests(_TempCase):
    """`fill_module_from_safetensors` over a GGUF file: the filler opens it through `tensor_container`."""

    def setUp(self) -> None:
        super().setUp()
        import torch

        from . import _gguf_test_fixtures as fx

        Q = fx.Q
        self.qkv = fx.make_tensor("blk.0.qkv.weight", Q.Q8_0, (6, 64), seed=1)
        self.proj = fx.make_tensor("blk.0.proj.weight", Q.Q4_K, (2, 256), seed=2)
        self.norm = fx.make_tensor("blk.0.norm.weight", Q.F32, (64,), seed=3)
        self.bias = fx.make_tensor("blk.0.proj.bias", Q.F16, (2,), seed=4)
        self.path = fx.write_gguf(self.root / "tiny.gguf", [self.qkv, self.proj, self.norm, self.bias])
        self.logger = logging.getLogger("test.runtime.safetensors_stream.gguf_fill")
        qkv_rows, proj_rows = self.qkv.raw.shape, self.proj.raw.shape

        class Block(torch.nn.Module):
            def __init__(self, split: bool) -> None:
                super().__init__()
                if split:
                    for part in ("q", "k", "v"):
                        self.register_buffer(f"{part}_weight", torch.zeros(qkv_rows[0] // 3, qkv_rows[1], dtype=torch.uint8))
                else:
                    self.register_buffer("qkv_weight", torch.zeros(qkv_rows, dtype=torch.uint8))
                self.register_buffer("proj_weight", torch.zeros(proj_rows, dtype=torch.uint8))
                self.norm = torch.nn.Parameter(torch.zeros(64))
                self.bias = torch.nn.Parameter(torch.zeros(2, dtype=torch.float16))

        self.block_class = Block

    def _meta(self, *, split: bool) -> Any:
        import torch

        with torch.device("meta"):
            return self.block_class(split)

    def _assert_bytes(self, value: Any, expected: Any) -> None:
        import numpy as np
        import torch

        self.assertTrue(np.array_equal(value.contiguous().view(torch.uint8).numpy().reshape(-1), expected.reshape(-1).view(np.uint8)))

    def test_one_to_one_keeps_quantized_rows_as_uint8(self) -> None:
        import torch

        names = {"blk.0.qkv.weight": "qkv_weight", "blk.0.proj.weight": "proj_weight", "blk.0.norm.weight": "norm", "blk.0.proj.bias": "bias"}
        module = self._meta(split=False)
        report = ss.fill_module_from_safetensors(module, [self.path], device="cpu", key_map=names.get, owner="Tiny", logger=self.logger)
        self.assertEqual(report.loaded, 4)
        self.assertEqual(report.bytes_read, sum(t.nbytes for t in (self.qkv, self.proj, self.norm, self.bias)))
        self.assertEqual(module.qkv_weight.dtype, torch.uint8)
        self._assert_bytes(module.qkv_weight, self.qkv.raw)
        self._assert_bytes(module.proj_weight, self.proj.raw)
        self.assertTrue(torch.equal(module.norm, torch.from_numpy(self.norm.reference())))
        self.assertEqual(module.bias.dtype, torch.float16)
        self._assert_bytes(module.bias, self.bias.raw)

    def test_one_to_many_splits_rows_in_file_order_one_tensor_at_a_time(self) -> None:
        reads: list[str] = []
        live: list[int] = []
        sources: list[weakref.ref] = []
        original = ss.StreamingTensorReader.read_tensor

        def read(reader: ss.StreamingTensorReader, span: ss.TensorSpan, **kwargs: Any) -> object:
            reads.append(span.name)
            live.append(sum(1 for ref in sources if ref() is not None))
            tensor = original(reader, span, **kwargs)
            sources.append(weakref.ref(tensor))
            return tensor

        renamed = {"blk.0.proj.weight": "proj_weight", "blk.0.norm.weight": "norm", "blk.0.proj.bias": "bias"}

        def declared(key: str) -> list[str]:
            return ["q_weight", "k_weight", "v_weight"] if key == "blk.0.qkv.weight" else [renamed[key]]

        def convert(key: str, tensor: Any) -> list[tuple[str, Any]]:
            if key == "blk.0.qkv.weight":
                # Whole quantized rows split along the output axis, cloned so the module owns its bytes.
                return [(name, part.clone()) for name, part in zip(declared(key), tensor.chunk(3, dim=0))]
            return [(renamed[key], tensor.clone())]

        module = self._meta(split=True)
        with patch.object(ss.StreamingTensorReader, "read_tensor", read):
            report = ss.fill_module_from_safetensors(module, [self.path], device="cpu", convert=convert, expected_keys_of=declared, owner="Tiny", logger=self.logger)
        self.assertEqual(report.loaded, 6)
        # File order is the writer's order; no earlier source is alive at the next read.
        self.assertEqual(reads, ["blk.0.qkv.weight", "blk.0.proj.weight", "blk.0.norm.weight", "blk.0.proj.bias"])
        self.assertEqual(live, [0, 0, 0, 0])
        rows = self.qkv.raw.shape[0] // 3
        for index, name in enumerate(("q_weight", "k_weight", "v_weight")):
            self._assert_bytes(getattr(module, name), self.qkv.raw[index * rows : (index + 1) * rows])


if __name__ == "__main__":
    unittest.main()
