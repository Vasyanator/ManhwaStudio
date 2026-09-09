"""
File: modules/ai_backend/inpaint/flux2_klein/test_streaming.py

Purpose:
Unit tests for `streaming.py`: the safetensors container contract the streaming
reader enforces before it reads a byte, the file-order rule, the layout decision,
the eligibility predicate, byte-level progress, and — with real torch — the
assumption the whole design rests on: that converting one key at a time gives
exactly what converting the whole checkpoint at once gives.

Main responsibilities:
- verify every malformed-header refusal is explicit and names the key
  (`parse_tensor_spans`) — an unknown dtype, a zero-length span, backwards
  offsets, a length that disagrees with the shape, a span past the end of file;
- verify tensors are iterated by DATA OFFSET, not alphabetically;
- verify `checkpoint_layout` decides once, refuses an unknown layout and gives a
  `model.diffusion_model.`-prefixed checkpoint its own message;
- verify `streaming_load_eligible` refuses a directory, a shard, a missing file,
  an offloaded placement, `low_cpu_mem_usage=False` and a CPU target;
- verify `_ByteProgress` rate-limits and always reports the final tensor;
- with real torch: per-key conversion == full-dict conversion, `torch.frombuffer`
  reproduces the bytes a genuine `safetensors.torch.save_file` container holds,
  and `load_transformer_streaming` fills a meta skeleton on a CPU target,
  refusing an incomplete checkpoint and tolerating an extra key.

Notes:
- The torch-free half runs everywhere. `StreamingRealTorchTests` is gated exactly
  like `PromptCacheRoundTripTests` in `test_prompt_cache.py`: torch is what
  performs the conversion and the meta fill, and nothing here needs a GPU.
"""

from __future__ import annotations

import importlib.util
import json
import struct
import tempfile
import unittest
from pathlib import Path

from modules.ai_backend.inpaint import flux2_klein as svc
from modules.ai_backend.inpaint.flux2_klein import streaming

from ._test_fixtures import _write_safetensors, _write_safetensors_with_data


#: One safetensors entry for a 2-element float32 tensor, as bytes.
_F32_PAIR = b"\x00\x01\x02\x03\x04\x05\x06\x07"


def _bfl_payload(index: int, count: int) -> bytes:
    """`count` distinct float32 bytes, deterministic per `index`."""
    return bytes(((index * 37 + position) % 251 for position in range(count * 4)))


class _StreamingCase(unittest.TestCase):
    """A `TestCase` with a temp directory that is removed afterwards."""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)


# ---------------------------------------------------------------------------
# Header -> spans
# ---------------------------------------------------------------------------
class TensorSpanParsingTests(_StreamingCase):
    def test_spans_come_back_in_file_order_not_alphabetical(self) -> None:
        path = self.root / "model.safetensors"
        _write_safetensors_with_data(
            path,
            {
                "zzz.weight": ("F32", (2,), _F32_PAIR),
                "aaa.weight": ("F32", (2,), _F32_PAIR),
                "mmm.weight": ("F32", (2,), _F32_PAIR),
            },
        )
        reader = streaming.StreamingSafetensorsReader(path)
        self.assertEqual(reader.keys_in_file_order(), ["zzz.weight", "aaa.weight", "mmm.weight"])
        self.assertEqual([span.start for span in reader.spans], [0, 8, 16])
        self.assertEqual(reader.total_tensor_bytes, 24)

    def test_metadata_entry_is_not_a_tensor(self) -> None:
        path = self.root / "model.safetensors"
        _write_safetensors_with_data(
            path, {"a.weight": ("F32", (2,), _F32_PAIR)}, metadata={"format": "pt"}
        )
        reader = streaming.StreamingSafetensorsReader(path)
        self.assertEqual(reader.keys_in_file_order(), ["a.weight"])

    def test_unknown_dtype_is_named(self) -> None:
        path = self.root / "model.safetensors"
        _write_safetensors(
            path, {"a.weight": {"dtype": "F8_E4M3", "shape": [2], "data_offsets": [0, 2]}}
        )
        with self.assertRaises(ValueError) as caught:
            streaming.StreamingSafetensorsReader(path)
        self.assertIn("a.weight", str(caught.exception))
        self.assertIn("F8_E4M3", str(caught.exception))

    def test_zero_length_tensor_is_refused_not_skipped(self) -> None:
        path = self.root / "model.safetensors"
        _write_safetensors(
            path, {"a.weight": {"dtype": "F32", "shape": [0], "data_offsets": [0, 0]}}
        )
        with self.assertRaises(ValueError) as caught:
            streaming.StreamingSafetensorsReader(path)
        self.assertIn("нулевая длина", str(caught.exception))

    def test_backwards_offsets_are_refused(self) -> None:
        path = self.root / "model.safetensors"
        _write_safetensors(
            path, {"a.weight": {"dtype": "F32", "shape": [2], "data_offsets": [16, 8]}}
        )
        with self.assertRaises(ValueError) as caught:
            streaming.StreamingSafetensorsReader(path)
        self.assertIn("вспять", str(caught.exception))

    def test_length_must_agree_with_shape_and_dtype(self) -> None:
        path = self.root / "model.safetensors"
        _write_safetensors(
            path, {"a.weight": {"dtype": "F32", "shape": [4], "data_offsets": [0, 8]}}
        )
        with self.assertRaises(ValueError) as caught:
            streaming.StreamingSafetensorsReader(path)
        self.assertIn("не совпадает с формой", str(caught.exception))

    def test_malformed_entries_are_refused(self) -> None:
        cases = {
            "not an object": "not-a-dict",
            "shape": {"dtype": "F32", "shape": "2", "data_offsets": [0, 8]},
            "offsets": {"dtype": "F32", "shape": [2], "data_offsets": [0]},
        }
        for label, entry in cases.items():
            with self.subTest(label):
                path = self.root / f"{label.replace(' ', '_')}.safetensors"
                _write_safetensors(path, {"a.weight": entry})
                with self.assertRaises(ValueError) as caught:
                    streaming.StreamingSafetensorsReader(path)
                self.assertIn("a.weight", str(caught.exception))

    def test_overlapping_spans_are_refused_naming_both_tensors(self) -> None:
        # The silent-wrong-result case: two expected weights aliasing the same
        # bytes both land in `loaded`, `missing` stays empty, and the model runs
        # on corrupted weights. Nothing downstream can notice.
        path = self.root / "model.safetensors"
        _write_safetensors(
            path,
            {
                "a.weight": {"dtype": "F32", "shape": [2], "data_offsets": [0, 8]},
                "b.weight": {"dtype": "F32", "shape": [2], "data_offsets": [4, 12]},
            },
        )
        with self.assertRaises(ValueError) as caught:
            streaming.StreamingSafetensorsReader(path)
        message = str(caught.exception)
        self.assertIn("a.weight", message)
        self.assertIn("b.weight", message)
        self.assertIn("Перекрывающиеся", message)

    def test_two_tensors_aliasing_the_same_bytes_are_refused(self) -> None:
        path = self.root / "model.safetensors"
        _write_safetensors(
            path,
            {
                "a.weight": {"dtype": "F32", "shape": [2], "data_offsets": [0, 8]},
                "b.weight": {"dtype": "F32", "shape": [2], "data_offsets": [0, 8]},
            },
        )
        with self.assertRaises(ValueError) as caught:
            streaming.StreamingSafetensorsReader(path)
        self.assertIn("Перекрывающиеся", str(caught.exception))

    def test_a_gap_between_spans_is_allowed(self) -> None:
        # Contiguity is NOT required: this reader seeks absolutely to every span,
        # so padding between tensors is read past, not misread. Refusing it would
        # reject an alignment-padded container that loads correctly.
        path = self.root / "model.safetensors"
        _write_safetensors_with_data(
            path,
            {
                "a.weight": ("F32", (2,), _F32_PAIR),
                "b.weight": ("F32", (2,), _F32_PAIR),
            },
        )
        header_len = struct.unpack("<Q", path.read_bytes()[:8])[0]
        body = path.read_bytes()[8 + header_len :]
        # Re-emit the same file with `b.weight` pushed 8 bytes further out and the
        # padding actually present, which is what a padded writer produces.
        padded_header = json.dumps(
            {
                "a.weight": {"dtype": "F32", "shape": [2], "data_offsets": [0, 8]},
                "b.weight": {"dtype": "F32", "shape": [2], "data_offsets": [16, 24]},
            }
        ).encode("utf-8")
        path.write_bytes(
            struct.pack("<Q", len(padded_header))
            + padded_header
            + body[:8]
            + b"\x00" * 8
            + body[8:16]
        )
        reader = streaming.StreamingSafetensorsReader(path)
        self.assertEqual(reader.keys_in_file_order(), ["a.weight", "b.weight"])
        self.assertEqual(reader.total_tensor_bytes, 16)

    def test_span_past_the_end_of_the_file_is_refused(self) -> None:
        path = self.root / "model.safetensors"
        _write_safetensors_with_data(path, {"a.weight": ("F32", (2,), _F32_PAIR)})
        path.write_bytes(path.read_bytes()[:-4])
        with self.assertRaises(ValueError) as caught:
            streaming.StreamingSafetensorsReader(path)
        self.assertIn("Обрезанный safetensors", str(caught.exception))

    def test_data_start_follows_the_header(self) -> None:
        path = self.root / "model.safetensors"
        _write_safetensors_with_data(path, {"a.weight": ("F32", (2,), _F32_PAIR)})
        header_len = struct.unpack("<Q", path.read_bytes()[:8])[0]
        reader = streaming.StreamingSafetensorsReader(path)
        self.assertEqual(reader.data_start, 8 + header_len)
        self.assertEqual(reader.file_size, 8 + header_len + 8)

    def test_the_header_only_reader_still_answers_the_old_question(self) -> None:
        # `read_safetensors_header` must keep its existing shape: the sibling that
        # also returns the data offset is an extension, not a replacement.
        path = self.root / "model.safetensors"
        _write_safetensors_with_data(path, {"a.weight": ("F32", (2,), _F32_PAIR)})
        header = svc.read_safetensors_header(path)
        header_again, data_start = svc.read_safetensors_header_and_data_start(path)
        self.assertEqual(header, header_again)
        self.assertEqual(data_start, 8 + struct.unpack("<Q", path.read_bytes()[:8])[0])


# ---------------------------------------------------------------------------
# Layout decision
# ---------------------------------------------------------------------------
class CheckpointLayoutTests(unittest.TestCase):
    source = Path("/models/klein.safetensors")

    def test_an_exact_key_match_means_no_conversion(self) -> None:
        keys = {"x_embedder.weight", "norm_out.linear.weight"}
        self.assertEqual(
            streaming.checkpoint_layout(keys, keys, source=self.source),
            streaming.CHECKPOINT_LAYOUT_DIFFUSERS,
        )

    def test_the_bfl_marker_key_selects_the_converter(self) -> None:
        self.assertEqual(
            streaming.checkpoint_layout(
                {streaming.FLUX2_BFL_MARKER_KEY, "img_in.weight"},
                {"single_stream_modulation.linear.weight", "x_embedder.weight"},
                source=self.source,
            ),
            streaming.CHECKPOINT_LAYOUT_BFL,
        )

    def test_a_prefixed_checkpoint_gets_its_own_refusal(self) -> None:
        with self.assertRaises(ValueError) as caught:
            streaming.checkpoint_layout(
                {"model.diffusion_model." + streaming.FLUX2_BFL_MARKER_KEY},
                {"single_stream_modulation.linear.weight"},
                source=self.source,
            )
        self.assertIn("model.diffusion_model.", str(caught.exception))

    def test_an_unknown_layout_is_refused_with_both_counts(self) -> None:
        with self.assertRaises(ValueError) as caught:
            streaming.checkpoint_layout(
                {"lora_unet_down.alpha", "lora_unet_up.alpha"},
                {"x_embedder.weight"},
                source=self.source,
            )
        message = str(caught.exception)
        self.assertIn("klein.safetensors", message)
        self.assertIn("lora_unet_down.alpha", message)

    def test_an_unknown_layout_name_is_refused_by_the_converter_driver(self) -> None:
        with self.assertRaises(ValueError):
            streaming.convert_tensor_to_diffusers("a.weight", object(), layout="lora", config={})


# ---------------------------------------------------------------------------
# Eligibility
# ---------------------------------------------------------------------------
class EligibilityTests(_StreamingCase):
    def setUp(self) -> None:
        super().setUp()
        self.checkpoint = self.root / "klein.safetensors"
        self.checkpoint.write_bytes(b"\x00" * 16)
        self.device_map = {"": "cuda:0"}

    def test_a_single_file_on_a_cuda_target_is_eligible(self) -> None:
        eligible, reason = streaming.streaming_load_eligible(
            self.checkpoint, low_cpu_mem_usage=True, device_map=self.device_map
        )
        self.assertTrue(eligible)
        self.assertEqual(reason, "")

    def test_every_refusal_names_its_reason(self) -> None:
        shard = self.root / "klein-00001-of-00002.safetensors"
        shard.write_bytes(b"\x00" * 16)
        folder = self.root / "transformer_folder"
        folder.mkdir()
        other = self.root / "klein.bin"
        other.write_bytes(b"\x00" * 16)
        cases = {
            "offload placement": (self.checkpoint, True, None, "device_map"),
            "partial device map": (self.checkpoint, True, {"blocks": "cuda:0"}, "на всю модель"),
            "no low_cpu_mem_usage": (self.checkpoint, False, self.device_map, "low_cpu_mem_usage"),
            "cpu target": (self.checkpoint, True, {"": "cpu"}, "CUDA/ROCm"),
            "directory": (folder, True, self.device_map, "каталог diffusers"),
            "missing file": (self.root / "nope.safetensors", True, self.device_map, "не найден"),
            "other container": (other, True, self.device_map, ".safetensors"),
            "one shard": (shard, True, self.device_map, "шардированного"),
        }
        for label, (source, low_cpu, device_map, fragment) in cases.items():
            with self.subTest(label):
                eligible, reason = streaming.streaming_load_eligible(
                    source, low_cpu_mem_usage=low_cpu, device_map=device_map
                )
                self.assertFalse(eligible)
                self.assertIn(fragment, reason)

    def test_a_model_class_declaring_fp32_modules_is_refused(self) -> None:
        class _Fp32Model:
            _keep_in_fp32_modules = ["norm"]

        with self.assertRaises(ValueError) as caught:
            streaming._require_plain_loading_contract(_Fp32Model)
        self.assertIn("_keep_in_fp32_modules", str(caught.exception))

    def test_a_model_class_ignoring_unexpected_keys_is_refused(self) -> None:
        class _IgnoringModel:
            _keys_to_ignore_on_load_unexpected = ["pos_embed"]

        with self.assertRaises(ValueError) as caught:
            streaming._require_plain_loading_contract(_IgnoringModel)
        self.assertIn("_keys_to_ignore_on_load_unexpected", str(caught.exception))


# ---------------------------------------------------------------------------
# Byte progress
# ---------------------------------------------------------------------------
class ByteProgressTests(unittest.TestCase):
    def setUp(self) -> None:
        self.now = 100.0
        self.seen: list[tuple[int, int, str]] = []

    def _clock(self) -> float:
        return self.now

    def _progress(self, done: int, total: int, key: str) -> None:
        self.seen.append((done, total, key))

    def test_reports_are_rate_limited_and_the_last_one_always_fires(self) -> None:
        reporter = streaming._ByteProgress(
            self._progress, 30, interval_seconds=2.0, clock=self._clock
        )
        reporter.advance(10, "a")
        self.now += 0.5
        reporter.advance(10, "b")
        self.now += 3.0
        reporter.advance(5, "c")
        reporter.advance(5, "d", final=True)
        self.assertEqual(self.seen, [(25, 30, "c"), (30, 30, "d")])
        self.assertEqual(reporter.done_bytes, 30)

    def test_a_raising_callback_does_not_break_the_load(self) -> None:
        def explode(done: int, total: int, key: str) -> None:
            raise RuntimeError("peer is gone")

        reporter = streaming._ByteProgress(explode, 10, interval_seconds=0.0, clock=self._clock)
        reporter.advance(10, "a", final=True)
        self.assertEqual(reporter.done_bytes, 10)

    def test_no_callback_still_counts(self) -> None:
        reporter = streaming._ByteProgress(None, 10, clock=self._clock)
        reporter.advance(4, "a")
        self.assertEqual(reporter.done_bytes, 4)


# ---------------------------------------------------------------------------
# Real torch: the assumption the design rests on
# ---------------------------------------------------------------------------
#: The five BFL keys the fixture checkpoint carries, in REVERSE alphabetical
#: order so that file order and key order genuinely disagree. They cover every
#: branch of the converter: a plain rename, the marker key's rename, the adaLN
#: `swap_scale_shift`, the fused-qkv `torch.chunk`, and a single-block rename.
_BFL_FIXTURE_SHAPES: tuple[tuple[str, tuple[int, int]], ...] = (
    ("single_stream_modulation.lin.weight", (3, 2)),
    ("single_blocks.0.linear1.weight", (5, 4)),
    ("img_in.weight", (4, 2)),
    ("final_layer.adaLN_modulation.1.weight", (4, 3)),
    ("double_blocks.0.img_attn.qkv.weight", (6, 4)),
)


class _RealTorchFixture(_StreamingCase):
    """Temp model tree plus the BFL fixture tensors; carries no test of its own.

    Gated on torch exactly like `PromptCacheRoundTripTests`. Nothing that builds
    on it needs a GPU: the target is `device_map={"": "cpu"}`, and the loader's
    `torch.cuda.synchronize` is guarded on the device type for that reason.
    """

    def setUp(self) -> None:
        super().setUp()
        if importlib.util.find_spec("torch") is None:  # pragma: no cover - host-dependent
            self.skipTest("torch is not installed")
        (self.root / "transformer").mkdir()
        (self.root / "transformer" / "config.json").write_text(
            json.dumps({"_class_name": "Flux2Transformer2DModel", "guidance_embeds": True}),
            encoding="utf-8",
        )
        self.checkpoint = self.root / "klein.safetensors"

    # -- fixtures ---------------------------------------------------------
    def _bfl_tensors(self) -> dict[str, object]:
        import torch

        tensors: dict[str, object] = {}
        for index, (name, shape) in enumerate(_BFL_FIXTURE_SHAPES):
            count = shape[0] * shape[1]
            values = [float(index * 100 + position) for position in range(count)]
            tensors[name] = torch.tensor(values, dtype=torch.float32).reshape(shape)
        return tensors

    def _write_bfl_checkpoint(self, tensors: dict[str, object], path: Path | None = None) -> Path:
        target = self.checkpoint if path is None else path
        _write_safetensors_with_data(
            target,
            {
                name: ("F32", tuple(tensor.shape), tensor.numpy().tobytes())
                for name, tensor in tensors.items()
            },
        )
        return target


class StreamingRealTorchTests(_RealTorchFixture):
    """The assumption the whole design rests on, plus the reader on real bytes."""

    def test_per_key_conversion_equals_one_full_dict_conversion(self) -> None:
        import torch
        from diffusers.loaders.single_file_utils import (
            convert_flux2_transformer_checkpoint_to_diffusers,
        )

        tensors = self._bfl_tensors()
        path = self._write_bfl_checkpoint(tensors)

        # The converter POPS what it is given, so the reference gets its own copy.
        reference = convert_flux2_transformer_checkpoint_to_diffusers(
            {name: tensor.clone() for name, tensor in tensors.items()}, config={}
        )

        streamed: dict[str, object] = {}
        reader = streaming.StreamingSafetensorsReader(path)
        with reader:
            for converted, span in streaming.iter_converted_transformer_tensors(
                reader, layout=streaming.CHECKPOINT_LAYOUT_BFL, config={}
            ):
                self.assertIn(span.name, tensors)
                streamed.update(converted)

        self.assertEqual(sorted(streamed), sorted(reference))
        for key, expected in reference.items():
            with self.subTest(key):
                self.assertTrue(torch.equal(streamed[key].to(torch.float32), expected))

    def test_frombuffer_reproduces_a_genuine_container_byte_for_byte(self) -> None:
        import torch
        from safetensors.torch import load_file, save_file

        tensors = {name: tensor for name, tensor in self._bfl_tensors().items()}
        path = self.root / "genuine.safetensors"
        save_file(tensors, str(path))

        reference = load_file(str(path))
        reader = streaming.StreamingSafetensorsReader(path)
        with reader:
            for span in reader.spans:
                with self.subTest(span.name):
                    streamed = reader.read_tensor(span)
                    self.assertEqual(tuple(streamed.shape), tuple(reference[span.name].shape))
                    self.assertTrue(torch.equal(streamed, reference[span.name]))
                    self.assertEqual(
                        streamed.numpy().tobytes(), reference[span.name].numpy().tobytes()
                    )

    def test_a_file_truncated_under_the_reader_reports_the_tensor(self) -> None:
        tensors = self._bfl_tensors()
        path = self._write_bfl_checkpoint(tensors)
        reader = streaming.StreamingSafetensorsReader(path)
        # Construction validated the spans against the file; the file then shrinks
        # (a replaced download, a full disk) and the read must say WHICH tensor.
        path.write_bytes(path.read_bytes()[: reader.data_start + 4])
        with reader:
            with self.assertRaises(ValueError) as caught:
                reader.read_tensor(reader.spans[0])
        self.assertIn(reader.spans[0].name, str(caught.exception))

    def test_reading_outside_the_context_manager_is_a_programming_error(self) -> None:
        path = self._write_bfl_checkpoint(self._bfl_tensors())
        reader = streaming.StreamingSafetensorsReader(path)
        with self.assertRaises(RuntimeError):
            reader.read_tensor(reader.spans[0])


# ---------------------------------------------------------------------------
# Real torch: the loader itself, on a CPU target
# ---------------------------------------------------------------------------
def _tiny_transformer_class():  # noqa: ANN202 - built lazily so the module imports without torch
    """Build a minimal `nn.Module` class shaped like the converted klein keys.

    It is a stand-in for `Flux2Transformer2DModel`: it offers the same three
    things the loader uses (`load_config`, `from_config`, and the absence of
    `_keep_in_fp32_modules` / `_keys_to_ignore_on_load_unexpected`) and its
    parameter names are exactly what the converter produces for
    `_BFL_FIXTURE_SHAPES`. Building the real 9B skeleton would need the real
    config and gains nothing: the loader's contract is about keys, devices and
    counts, not about attention.
    """
    import torch
    from torch import nn

    class _Attn(nn.Module):
        def __init__(self) -> None:
            super().__init__()
            self.to_q = nn.Linear(4, 2, bias=False)
            self.to_k = nn.Linear(4, 2, bias=False)
            self.to_v = nn.Linear(4, 2, bias=False)

    class _SingleAttn(nn.Module):
        def __init__(self) -> None:
            super().__init__()
            self.to_qkv_mlp_proj = nn.Linear(4, 5, bias=False)

    class _Block(nn.Module):
        def __init__(self, attn: nn.Module) -> None:
            super().__init__()
            self.attn = attn

    class _Linear(nn.Module):
        def __init__(self, fan_in: int, fan_out: int) -> None:
            super().__init__()
            self.linear = nn.Linear(fan_in, fan_out, bias=False)

    class _TinyTransformer(nn.Module):
        """A meta-fillable model whose state dict matches the converted fixture."""

        _keep_in_fp32_modules = None
        _keys_to_ignore_on_load_unexpected = None
        last_config: dict[str, object] = {}

        def __init__(self) -> None:
            super().__init__()
            self.x_embedder = nn.Linear(2, 4, bias=False)
            self.transformer_blocks = nn.ModuleList([_Block(_Attn())])
            self.single_transformer_blocks = nn.ModuleList([_Block(_SingleAttn())])
            self.norm_out = _Linear(3, 4)
            self.single_stream_modulation = _Linear(2, 3)
            # Non-persistent: it stays out of `state_dict()` (so it is not an
            # expected weight) but `init_empty_weights` leaves it on the host,
            # which is exactly the re-homing case the loader must handle.
            self.register_buffer("rope_freqs", torch.zeros(4), persistent=False)

        @classmethod
        def load_config(cls, config_dir: str, **_kwargs: object) -> dict[str, object]:
            return json.loads((Path(config_dir) / "config.json").read_text(encoding="utf-8"))

        @classmethod
        def from_config(cls, config: dict[str, object]) -> "_TinyTransformer":
            _TinyTransformer.last_config = dict(config)
            return cls()

    return _TinyTransformer


class StreamingLoaderTests(_RealTorchFixture):
    """`load_transformer_streaming` end to end against a meta skeleton on the CPU."""

    def _load(self, **kwargs: object):  # noqa: ANN202 - returns the test double
        model_cls = _tiny_transformer_class()
        return model_cls, streaming.load_transformer_streaming(
            model_cls,
            kwargs.pop("source", self.checkpoint),
            dtype=kwargs.pop("dtype", None),
            device_map={"": "cpu"},
            low_cpu_mem_usage=True,
            **kwargs,
        )

    def test_every_weight_lands_and_nothing_stays_on_meta(self) -> None:
        import torch

        tensors = self._bfl_tensors()
        self._write_bfl_checkpoint(tensors)
        model_cls, model = self._load()

        self.assertFalse(model.training)
        self.assertFalse(any(parameter.is_meta for parameter in model.parameters()))
        self.assertEqual(model.rope_freqs.device.type, "cpu")
        # `guidance_embeds` is forced off: klein has no guidance embedder, and the
        # fixture config deliberately says `true`.
        self.assertIs(model_cls.last_config["guidance_embeds"], False)
        # The fused qkv really was split three ways, in order.
        fused = tensors["double_blocks.0.img_attn.qkv.weight"]
        block = model.transformer_blocks[0].attn
        for index, projection in enumerate((block.to_q, block.to_k, block.to_v)):
            with self.subTest(index):
                self.assertTrue(torch.equal(projection.weight, fused[index * 2 : index * 2 + 2]))

    def test_the_adaln_weight_is_swapped_exactly_once(self) -> None:
        import torch
        from diffusers.loaders.single_file_utils import swap_scale_shift

        tensors = self._bfl_tensors()
        self._write_bfl_checkpoint(tensors)
        _, model = self._load()
        expected = swap_scale_shift(tensors["final_layer.adaLN_modulation.1.weight"], 0)
        self.assertTrue(torch.equal(model.norm_out.linear.weight, expected))

    def test_a_diffusers_layout_file_is_loaded_without_conversion(self) -> None:
        import torch

        model_cls = _tiny_transformer_class()
        skeleton = model_cls()
        diffusers_tensors = {
            name: torch.arange(tensor.numel(), dtype=torch.float32).reshape(tensor.shape)
            for name, tensor in skeleton.state_dict().items()
        }
        _write_safetensors_with_data(
            self.checkpoint,
            {
                name: ("F32", tuple(tensor.shape), tensor.numpy().tobytes())
                for name, tensor in diffusers_tensors.items()
            },
        )
        model = streaming.load_transformer_streaming(
            model_cls,
            self.checkpoint,
            dtype=None,
            device_map={"": "cpu"},
            low_cpu_mem_usage=True,
        )
        # An unconverted load: the adaLN weight must NOT have been swapped.
        self.assertTrue(
            torch.equal(
                model.norm_out.linear.weight, diffusers_tensors["norm_out.linear.weight"]
            )
        )

    def test_an_incomplete_checkpoint_names_the_missing_weights(self) -> None:
        tensors = self._bfl_tensors()
        tensors.pop("img_in.weight")
        self._write_bfl_checkpoint(tensors)
        with self.assertRaises(RuntimeError) as caught:
            self._load()
        message = str(caught.exception)
        self.assertIn("x_embedder.weight", message)
        self.assertIn("klein.safetensors", message)

    def test_an_extra_key_is_logged_not_raised(self) -> None:
        import torch

        tensors = self._bfl_tensors()
        tensors["img_in_extra.weight"] = torch.zeros((2, 2), dtype=torch.float32)
        self._write_bfl_checkpoint(tensors)
        with self.assertLogs(streaming.log, level="WARNING") as captured:
            _, model = self._load()
        self.assertIn("лишних ключей", "\n".join(captured.output))
        self.assertFalse(any(parameter.is_meta for parameter in model.parameters()))

    def test_dtype_is_applied_to_every_floating_weight(self) -> None:
        import torch

        self._write_bfl_checkpoint(self._bfl_tensors())
        _, model = self._load(dtype=torch.bfloat16)
        for name, parameter in model.named_parameters():
            with self.subTest(name):
                self.assertEqual(parameter.dtype, torch.bfloat16)

    def test_byte_progress_reaches_the_total(self) -> None:
        self._write_bfl_checkpoint(self._bfl_tensors())
        seen: list[tuple[int, int, str]] = []
        self._load(progress=lambda done, total, key: seen.append((done, total, key)))
        self.assertTrue(seen)
        done, total, _ = seen[-1]
        self.assertEqual(done, total)

    def test_a_missing_transformer_config_is_refused(self) -> None:
        (self.root / "transformer" / "config.json").unlink()
        self._write_bfl_checkpoint(self._bfl_tensors())
        with self.assertRaises(FileNotFoundError):
            self._load()

    def test_an_aliasing_header_is_refused_before_the_model_is_built(self) -> None:
        # The regression FIX 3 exists for: a header that gives two EXPECTED keys
        # the same byte range used to pass every per-entry check, so both landed
        # in `loaded`, `missing` came back empty and the model was built and
        # returned on corrupted weights. The refusal must come from the reader,
        # i.e. before `from_config` — a model half-built on aliased weights is
        # exactly the state that must never exist.
        # The tensor bytes are really there, so the truncation check cannot be
        # what refuses this file: only the overlap check can.
        _write_safetensors(
            self.checkpoint,
            {
                "img_in.weight": {"dtype": "F32", "shape": [4, 2], "data_offsets": [0, 32]},
                "single_stream_modulation.lin.weight": {
                    "dtype": "F32",
                    "shape": [3, 2],
                    "data_offsets": [8, 32],
                },
            },
        )
        self.checkpoint.write_bytes(self.checkpoint.read_bytes() + bytes(32))
        model_cls = _tiny_transformer_class()
        with self.assertRaises(ValueError) as caught:
            streaming.load_transformer_streaming(
                model_cls,
                self.checkpoint,
                dtype=None,
                device_map={"": "cpu"},
                low_cpu_mem_usage=True,
            )
        self.assertIn("Перекрывающиеся", str(caught.exception))
        # `from_config` records the config it was handed; an untouched `{}` proves
        # the skeleton was never constructed.
        self.assertEqual(model_cls.last_config, {})

    def test_an_fp8_scaled_checkpoint_is_refused_before_any_read(self) -> None:
        # F8 dtype tokens are deliberately absent from `SAFETENSORS_DTYPES`, so this
        # also pins the ORDER: the fp8 wording must win over "unsupported dtype".
        _write_safetensors(
            self.checkpoint,
            {
                "img_in.weight": {"dtype": "F8_E4M3", "shape": [8], "data_offsets": [0, 8]},
                "img_in.weight_scale": {"dtype": "F32", "shape": [1], "data_offsets": [8, 12]},
            },
        )
        with self.assertRaises(ValueError) as caught:
            self._load()
        self.assertIn("fp8_scaled", str(caught.exception))


if __name__ == "__main__":  # pragma: no cover - manual run
    unittest.main()
