"""
File: modules/ai_backend/runtime/large_models/test_inventory.py

Purpose:
Unit tests for `inventory.py` and the spec validation it relies on: block
grouping and natural order, non-block weights, device itemsize conversion,
dropped tensors, 1:N state keys, the `fragmented` flag (interleaved blocks,
a block split across files), the load transient (`convert_bytes` from the
conversion run on `meta`: casts and copies count, aliases and views do not),
and every refusal; a GGUF file (`GgufInventoryTests`): quantized U8 rows and
plain tensors sized by their byte spans, blocks contiguous.

Notes:
Header-only safetensors files in a temporary directory; nothing is read
beyond the headers. The GGUF case writes a tiny real file with
`runtime._gguf_test_fixtures` and skips without the `gguf` package.
"""

from __future__ import annotations

import importlib.util
import tempfile
import unittest
from pathlib import Path

try:
    import torch as _torch
except ImportError:  # the meta-conversion tests skip without torch
    _torch = None

from ._test_fixtures import GIB, MIB, write_header_only
from .inventory import build_inventory
from .spec import ComponentSpec, LargeModelSpec, PhaseSpec, PhaseWork, RequestShape, Role, natural_block_order


def _block_of(key: str) -> str | None:
    parts = key.split(".")
    return ".".join(parts[:2]) if parts[0] == "blocks" else None


def _component(**overrides: object) -> ComponentSpec:
    fields = {"name": "dit", "role": Role.DIT, "files": lambda options, fmt: [], "block_of": _block_of}
    fields.update(overrides)
    return ComponentSpec(**fields)  # type: ignore[arg-type]


class InventoryTests(unittest.TestCase):
    def setUp(self) -> None:
        self._dir = tempfile.TemporaryDirectory()
        self.addCleanup(self._dir.cleanup)
        self.root = Path(self._dir.name)

    def test_blocks_grouped_in_natural_order_with_non_block_weights(self) -> None:
        path = write_header_only(self.root / "m.safetensors", [("embed.w", "BF16", 8 * MIB)] + [(f"blocks.{i}.{part}", "BF16", MIB) for i in (0, 1, 2, 10, 9) for part in ("a", "b")])
        inventory = build_inventory(_component(), [path])
        self.assertEqual(inventory.block_names, ("blocks.0", "blocks.1", "blocks.2", "blocks.9", "blocks.10"))
        self.assertEqual(inventory.bytes_by_block["blocks.9"], 4 * MIB)
        self.assertEqual(inventory.non_block_bytes, 16 * MIB)
        self.assertEqual(inventory.bytes_total, 16 * MIB + 5 * 4 * MIB)
        self.assertEqual(inventory.file_bytes_total, inventory.bytes_total)
        self.assertEqual(inventory.resident_bytes(2), 16 * MIB + 8 * MIB)
        self.assertEqual(inventory.remainder_bytes(2), 12 * MIB)
        self.assertEqual(inventory.max_tensor_bytes, 16 * MIB)
        self.assertFalse(inventory.fragmented)

    def test_device_itemsize_and_dropped_tensors(self) -> None:
        path = write_header_only(self.root / "m.safetensors", [("blocks.0.w", "BF16", GIB // 2), ("unused.w", "F32", MIB)])
        component = _component(device_itemsize=lambda key, token: 4, state_keys_of=lambda key: () if key.startswith("unused") else (key,))
        inventory = build_inventory(component, [path])
        self.assertEqual(inventory.bytes_total, 2 * GIB, "bf16 file cast to fp32 on the device")
        self.assertEqual(inventory.file_bytes_total, 1 * GIB)
        self.assertEqual(inventory.file_bytes_for(1 * GIB), GIB // 2)
        self.assertEqual(inventory.non_block, ())

    def test_one_file_tensor_to_several_state_keys(self) -> None:
        path = write_header_only(self.root / "m.safetensors", [("blocks.0.qkv", "BF16", 3 * MIB)])
        component = _component(state_keys_of=lambda key: tuple(key.replace("qkv", part) for part in ("q", "k", "v")))
        inventory = build_inventory(component, [path])
        (span,) = inventory.blocks["blocks.0"]
        self.assertEqual(span.state_keys, ("blocks.0.q", "blocks.0.k", "blocks.0.v"))
        self.assertEqual(span.device_bytes, 6 * MIB)

    def test_interleaved_blocks_are_fragmented(self) -> None:
        path = write_header_only(self.root / "m.safetensors", [("blocks.0.a", "U8", MIB), ("blocks.1.a", "U8", MIB), ("blocks.0.b", "U8", MIB), ("blocks.1.b", "U8", MIB)])
        self.assertTrue(build_inventory(_component(), [path]).fragmented)

    def test_block_interrupted_by_non_block_weight_is_fragmented(self) -> None:
        path = write_header_only(self.root / "m.safetensors", [("blocks.0.a", "U8", MIB), ("embed.w", "U8", MIB), ("blocks.0.b", "U8", MIB)])
        self.assertTrue(build_inventory(_component(), [path]).fragmented)

    def test_block_split_across_files_is_fragmented(self) -> None:
        first = write_header_only(self.root / "a.safetensors", [("blocks.0.a", "U8", MIB)])
        second = write_header_only(self.root / "b.safetensors", [("blocks.0.b", "U8", MIB), ("blocks.1.a", "U8", MIB)])
        inventory = build_inventory(_component(), [first, second])
        self.assertTrue(inventory.fragmented)
        self.assertEqual(inventory.bytes_by_block["blocks.0"], 2 * MIB)

    def test_blocks_in_separate_files_are_contiguous(self) -> None:
        first = write_header_only(self.root / "a.safetensors", [("blocks.0.a", "U8", MIB), ("blocks.0.b", "U8", MIB)])
        second = write_header_only(self.root / "b.safetensors", [("blocks.1.a", "U8", MIB)])
        self.assertFalse(build_inventory(_component(), [first, second]).fragmented)

    def test_refusals(self) -> None:
        path = write_header_only(self.root / "m.safetensors", [("blocks.0.w", "U8", MIB)])
        with self.assertRaisesRegex(ValueError, "не указано"):
            build_inventory(_component(), [])
        with self.assertRaisesRegex(ValueError, "разные блоки"):
            build_inventory(_component(state_keys_of=lambda key: ("blocks.0.x", "blocks.1.x")), [path])
        duplicate = write_header_only(self.root / "d.safetensors", [("blocks.0.w", "U8", MIB), ("blocks.0.v", "U8", MIB)])
        with self.assertRaisesRegex(ValueError, "двух тензоров"):
            build_inventory(_component(state_keys_of=lambda key: ("blocks.0.same",)), [duplicate])
        with self.assertRaisesRegex(ValueError, "ни одного загружаемого"):
            build_inventory(_component(state_keys_of=lambda key: ()), [path])
        broken = self.root / "broken.safetensors"
        broken.write_bytes(b"\x01")
        with self.assertRaises(ValueError):
            build_inventory(_component(), [broken])
        with self.assertRaises(ValueError):
            build_inventory(_component(), [self.root / "missing.safetensors"])

    def test_load_transient_without_conversion_is_the_largest_file_tensor(self) -> None:
        path = write_header_only(self.root / "m.safetensors", [("embed.w", "BF16", 8 * MIB), ("blocks.0.w", "BF16", MIB)])
        inventory = build_inventory(_component(), [path])
        self.assertEqual({span.convert_bytes for span in inventory.non_block}, {0})
        self.assertEqual(inventory.load_host_bytes, 16 * MIB, "the read buffer alone")

    @unittest.skipIf(_torch is None, "torch not installed")
    def test_load_transient_counts_only_conversion_outputs_that_do_not_alias_the_source(self) -> None:
        """Measured on gfx1201: a bf16 embedding kept as bf16 costs its read buffer once, an f16 one cast to bf16 twice."""
        assert _torch is not None
        torch = _torch
        path = write_header_only(self.root / "m.safetensors", [("embed.w", "F16", 8 * MIB), ("same.w", "BF16", 4 * MIB), ("blocks.0.qkv", "BF16", 3 * MIB), ("blocks.0.t", "BF16", MIB)])

        def state_keys_of(key: str) -> tuple[str, ...]:
            return tuple(key.replace("qkv", part) for part in ("q", "k", "v")) if key.endswith("qkv") else (key,)

        def convert(key: str, tensor: object) -> object:
            assert isinstance(tensor, torch.Tensor)
            if key.endswith("qkv"):
                yield from zip(state_keys_of(key), tensor.chunk(3))  # views of the read buffer
            elif key.endswith(".t"):
                yield key, tensor.view(1024, -1).t().contiguous()  # a real copy, same dtype
            else:
                yield key, tensor.to(torch.bfloat16)  # a cast for F16, the source itself for BF16

        component = _component(state_keys_of=state_keys_of, device_itemsize=lambda key, token: 2, convert=convert)
        inventory = build_inventory(component, [path])
        spans = {span.key: span for span in [*inventory.non_block, *inventory.blocks["blocks.0"]]}
        self.assertEqual(spans["embed.w"].convert_bytes, 16 * MIB)
        self.assertEqual(spans["same.w"].convert_bytes, 0)
        self.assertEqual(spans["blocks.0.qkv"].convert_bytes, 0)
        self.assertEqual(spans["blocks.0.t"].convert_bytes, 2 * MIB)
        self.assertEqual(inventory.load_host_bytes, 32 * MIB, "the F16 embedding: read buffer + its bf16 copy")

        def broken(key: str, tensor: object) -> object:
            raise RuntimeError("not meta-safe")

        with self.assertRaisesRegex(ValueError, "meta"):
            build_inventory(_component(convert=broken), [path])

    def test_custom_block_order_must_be_a_permutation(self) -> None:
        path = write_header_only(self.root / "m.safetensors", [("blocks.0.w", "U8", MIB), ("blocks.1.w", "U8", MIB)])
        reverse = build_inventory(_component(block_order=lambda names: sorted(names, reverse=True)), [path])
        self.assertEqual(reverse.block_names, ("blocks.1", "blocks.0"))
        with self.assertRaisesRegex(ValueError, "block_order"):
            build_inventory(_component(block_order=lambda names: list(names)[:1]), [path])


class SpecValidationTests(unittest.TestCase):
    def _phase(self, name: str, uses: frozenset[str], compute: str) -> PhaseSpec:
        return PhaseSpec(name, uses, compute, lambda shape, options: PhaseWork())

    def test_spec_refuses_inconsistent_declarations(self) -> None:
        component = _component()
        with self.assertRaisesRegex(ValueError, "compute"):
            self._phase("denoise", frozenset({"dit"}), "ae")
        with self.assertRaisesRegex(ValueError, "неизвестные"):
            LargeModelSpec("toy", (component,), (self._phase("denoise", frozenset({"ae"}), "ae"),))
        with self.assertRaisesRegex(ValueError, "зарезервировано"):
            LargeModelSpec("toy", (component,), (self._phase("idle", frozenset({"dit"}), "dit"),))
        with self.assertRaisesRegex(ValueError, "повторяющиеся имена компонентов"):
            LargeModelSpec("toy", (component, component), (self._phase("denoise", frozenset({"dit"}), "dit"),))
        with self.assertRaisesRegex(ValueError, "ни одного устройства"):
            _component(exec_devices_allowed=frozenset())

    def test_request_shape_and_work_validation(self) -> None:
        with self.assertRaises(ValueError):
            RequestShape(0, 512)
        with self.assertRaises(ValueError):
            RequestShape(512, 512, references=((0, 4),))
        with self.assertRaises(ValueError):
            PhaseWork(passes=0)
        with self.assertRaises(ValueError):
            PhaseWork(tokens=-1)

    def test_natural_order(self) -> None:
        self.assertEqual(natural_block_order(["single.10", "single.2", "content.image.1", "content.image.0"]), ["content.image.0", "content.image.1", "single.2", "single.10"])


@unittest.skipUnless(importlib.util.find_spec("gguf") is not None, "gguf is not installed")
class GgufInventoryTests(unittest.TestCase):
    def setUp(self) -> None:
        self._dir = tempfile.TemporaryDirectory()
        self.addCleanup(self._dir.cleanup)
        self.root = Path(self._dir.name)

    def test_a_gguf_component_is_sized_by_its_byte_spans(self) -> None:
        from .. import _gguf_test_fixtures as fx

        tensors = [
            fx.make_tensor("embed.w", fx.Q.BF16, (4, 64), seed=1),
            fx.make_tensor("blocks.0.a", fx.Q.Q4_K, (8, 256), seed=2),
            fx.make_tensor("blocks.0.norm", fx.Q.F32, (32,), seed=3),
            fx.make_tensor("blocks.1.a", fx.Q.Q8_0, (8, 64), seed=4),
            fx.make_tensor("blocks.1.b", fx.Q.Q6_K, (2, 256), seed=5),
        ]
        path = fx.write_gguf(self.root / "m.gguf", tensors)
        inventory = build_inventory(_component(), [path])
        nbytes = {tensor.name: tensor.nbytes for tensor in tensors}
        self.assertEqual(inventory.block_names, ("blocks.0", "blocks.1"))
        self.assertFalse(inventory.fragmented)
        for spans in inventory.blocks.values():
            for entry in spans:
                with self.subTest(entry.key):
                    self.assertEqual(entry.device_bytes, nbytes[entry.key])
                    self.assertEqual(entry.file_bytes, nbytes[entry.key])
        self.assertEqual(inventory.bytes_by_block["blocks.1"], nbytes["blocks.1.a"] + nbytes["blocks.1.b"])
        self.assertEqual(inventory.non_block_bytes, nbytes["embed.w"])
        self.assertEqual(inventory.file_bytes_total, sum(nbytes.values()))
        (quantized, norm) = inventory.blocks["blocks.0"]
        self.assertEqual((quantized.span.dtype_token, quantized.span.quant_type), ("U8", int(fx.Q.Q4_K)))
        self.assertIsNone(norm.span.quant_type)


if __name__ == "__main__":
    unittest.main()
