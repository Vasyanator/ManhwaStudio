"""
File: modules/ai_backend/inpaint/flux3_klein/test_gguf.py

Purpose:
Contract tests of the FLUX.3 klein GGUF DiT and GGUF text encoder (D6-D9):
- a tiny real `Flux3Dit` written as GGUF (block linears Q8_0 / Q4_0 / Q5_0,
  the rest BF16 / F32, like unsloth's export) runs through the REAL
  `LargeModelManager` in every «Видеопамять» x «Оперативная память» step pair
  and computes exactly what its bf16 safetensors twin (the dequantized
  weights) computes;
- a tiny `Qwen3VLTextModel` written as a `qwen3vl` GGUF (llama.cpp names, 36
  blocks of which 33 are kept) loads through the spec's name bridge and
  encodes exactly what its bf16 shard twin encodes, in every step pair;
- the header checks: `loading.require_gguf_dit` and
  `text_encoder.require_gguf_text_encoder` refuse foreign, incomplete,
  mis-shaped or mis-declared files naming the wire field, before anything is
  loaded;
- the RECORDED headers of the real files (`fixtures/flux3_gguf_headers.json`,
  no tensor data) pass those checks against the recorded package configs;
- the spec facts: residency keys of both handles (D9), format rows, compute
  path, the encode work variant (D8), the encoder files of the PLANNED format.

Notes:
CPU only; every file goes to a temporary directory removed by the class. The
twins hold `runtime.gguf_layers.dequantize(...)` cast to bf16, i.e. exactly the
transient weights the GGUF layers compute with, so equality is exact (the
dequantization error itself is `runtime/test_gguf_layers.py`'s subject). The
AE meta build is patched to the tiny AE of `test_large_model.py`.
"""

from __future__ import annotations

import json
import tempfile
import unittest
from collections.abc import Callable
from pathlib import Path
from typing import Any
from unittest.mock import patch

import numpy as np
import torch
from gguf import MODEL_ARCH, GGMLQuantizationType, get_tensor_name_map, quants
from gguf.constants import GGML_QUANT_SIZES
from safetensors.torch import save_file
from transformers import Qwen3VLConfig
from transformers.models.qwen3_vl.modeling_qwen3_vl import Qwen3VLTextModel

from ...runtime._gguf_test_fixtures import FixtureTensor, write_gguf
from ...runtime.gguf_layers import GgufEmbedding, GgufLinear, dequantize
from ...runtime.large_models.manager import LargeModelManager, MemoryLimitsStore
from ...runtime.large_models.tiers import RAM_ORDER, VRAM_ORDER, RamStep, Steps, VramStep
from ...runtime.residency.arbiter import ResidencyArbiter
from ...runtime.safetensors_stream import TensorSpan
from . import autoencoder, large_model, loading
from . import text_encoder as te
from ._service_test_fixtures import FakeProbes
from .config import ENCODER_BF16_SOURCE, EncoderSource, GgufDit, PackageConfig, WeightFormat, read_package_config
from .dit import Flux3Dit
from .params import AeDtype, EncoderMode, Flux3Options, Fp8Compute
from .test_dit import make_pack, tiny_dit_config
from .test_large_model import _TINY_AE, SHAPE, _meta_ae, _tables
from .test_loading import _config_json, _source_model

Q = GGMLQuantizationType

#: The recorded real headers (`fixtures/flux3_gguf_headers.json`).
HEADERS_FILE = Path(__file__).resolve().parent / "fixtures" / "flux3_gguf_headers.json"

#: Block-linear types of the tiny GGUFs, cycled (rows are whole 32-blocks).
_CYCLE = (Q.Q8_0, Q.Q4_0, Q.Q5_0)

#: Tiny GGUF-friendly Qwen3-VL text sizes (every row a multiple of 32).
_ENC_HIDDEN = 32
_ENC_FFN = 64
_ENC_LAYERS = 36
_ENC_VOCAB = 64
_ENC_PAD = 62

#: Every concrete step pair.
_ALL_STEPS = tuple(Steps(vram, ram) for vram in VRAM_ORDER for ram in RAM_ORDER)


def _encode(name: str, qtype: GGMLQuantizationType, values: torch.Tensor) -> tuple[FixtureTensor, torch.Tensor]:
    """`values` stored as `qtype` and its twin: the weight the backend computes with (F32 as is, BF16 exact, quantized = `dequantize` in bf16)."""
    array = np.ascontiguousarray(values.detach().float().numpy())
    if qtype == Q.F32:
        return FixtureTensor(name, qtype, tuple(values.shape), array.astype(np.float32)), values.detach().float().clone()
    raw = np.ascontiguousarray(quants.quantize(array, qtype))
    fixture = FixtureTensor(name, qtype, tuple(values.shape), raw)
    if qtype == Q.BF16:
        return fixture, torch.from_numpy(fixture.reference()).to(torch.bfloat16)
    return fixture, dequantize(torch.from_numpy(raw), int(qtype), row_elements=int(values.shape[-1])).to(torch.bfloat16)


def _dit_gguf(model: Flux3Dit) -> tuple[list[FixtureTensor], dict[str, torch.Tensor]]:
    """The tiny DiT as GGUF tensors (block linears quantized, other 2-D weights BF16, 1-D F32) and its bf16 twin state."""
    tensors: list[FixtureTensor] = []
    twin: dict[str, torch.Tensor] = {}
    cycle = 0
    for name, value in model.state_dict().items():
        if value.dim() == 1:
            qtype = Q.F32
        elif large_model.dit_block_of(name) is not None and value.shape[-1] % 32 == 0:
            qtype = _CYCLE[cycle % len(_CYCLE)]
            cycle += 1
        else:
            qtype = Q.BF16
        fixture, weight = _encode(name, qtype, value)
        tensors.append(fixture)
        twin[name] = weight.contiguous()
    return tensors, twin


def _encoder_config() -> Qwen3VLConfig:
    """A tiny Qwen3-VL with GGUF-friendly text sizes (the vision tower exists only in the config)."""
    return Qwen3VLConfig(
        text_config={
            "hidden_size": _ENC_HIDDEN,
            "intermediate_size": _ENC_FFN,
            "num_hidden_layers": _ENC_LAYERS,
            "num_attention_heads": 2,
            "num_key_value_heads": 1,
            "head_dim": 16,
            "vocab_size": _ENC_VOCAB,
            "max_position_embeddings": 4096,
            "rope_theta": 5000000,
            "rope_scaling": {"rope_type": "default", "mrope_section": [4, 2, 2], "mrope_interleaved": True},
            "tie_word_embeddings": True,
        },
        vision_config={"depth": 1, "hidden_size": 16, "intermediate_size": 32, "num_heads": 2, "out_hidden_size": _ENC_HIDDEN, "num_position_embeddings": 16, "deepstack_visual_indexes": [0]},
        tie_word_embeddings=True,
        attn_implementation="sdpa",
    )


def _encoder_metadata(**overrides: Any) -> dict[str, Any]:
    """The `qwen3vl.*` hyper parameters of `_encoder_config` (what llama.cpp writes)."""
    data: dict[str, Any] = {
        "qwen3vl.block_count": _ENC_LAYERS,
        "qwen3vl.embedding_length": _ENC_HIDDEN,
        "qwen3vl.feed_forward_length": _ENC_FFN,
        "qwen3vl.attention.head_count": 2,
        "qwen3vl.attention.head_count_kv": 1,
        "qwen3vl.attention.key_length": 16,
    }
    data.update(overrides)
    return data


def _encoder_gguf(config: Qwen3VLConfig) -> tuple[list[FixtureTensor], dict[str, torch.Tensor]]:
    """A random full-depth text model as llama.cpp GGUF tensors and the twin's checkpoint state (`model.language_model.*`).

    GGUF names come from the `gguf` package's own QWEN3 map (independent of
    `runtime.qwen3_gguf`, the code under test).
    """
    torch.manual_seed(77)
    model = Qwen3VLTextModel(config.text_config).eval()
    with torch.no_grad():
        for parameter in model.parameters():
            parameter.normal_(0.0, 0.2)
    name_map = get_tensor_name_map(MODEL_ARCH.QWEN3, _ENC_LAYERS)
    tensors: list[FixtureTensor] = []
    twin: dict[str, torch.Tensor] = {}
    cycle = 0
    for key, value in model.state_dict().items():
        gguf_name = name_map.get_name(f"model.{key}", try_suffixes=(".weight", ".bias"))
        assert gguf_name is not None, key
        if value.dim() == 1:
            qtype = Q.F32
        else:
            qtype = _CYCLE[cycle % len(_CYCLE)]
            cycle += 1
        fixture, weight = _encode(gguf_name, qtype, value)
        tensors.append(fixture)
        twin[f"model.language_model.{key}"] = weight.contiguous()
    return tensors, twin


def _write_package(root: Path) -> tuple[Path, Path]:
    """A tiny package whose bf16 DiT and encoder shards are the twins of a GGUF DiT and a GGUF encoder; returns their paths."""
    dit_tensors, dit_twin = _dit_gguf(_source_model())
    (root / "transformer_gguf").mkdir(parents=True)
    dit_gguf = write_gguf(root / "transformer_gguf" / "tiny-Q.gguf", dit_tensors, architecture="flux")
    save_file(dit_twin, str(root / "model.safetensors"))
    (root / "config.json").write_text(json.dumps(_config_json(None)), encoding="utf-8")
    torch.manual_seed(3)
    ae = autoencoder.AutoEncoder(_TINY_AE)
    save_file({key: value.contiguous() for key, value in ae.state_dict().items()}, str(root / "ae.safetensors"))
    encoder_dir = root / "text_encoder"
    encoder_dir.mkdir()
    config = _encoder_config()
    config.save_pretrained(encoder_dir)
    enc_tensors, enc_twin = _encoder_gguf(config)
    shard = "model-00001-of-00001.safetensors"
    save_file(enc_twin, str(encoder_dir / shard))
    (encoder_dir / "model.safetensors.index.json").write_text(json.dumps({"weight_map": {key: shard for key in enc_twin}}), encoding="utf-8")
    (root / "text_encoder_gguf").mkdir()
    encoder_gguf = write_gguf(root / "text_encoder_gguf" / "tiny-enc.gguf", enc_tensors, architecture="qwen3vl", metadata=_encoder_metadata())
    return dit_gguf, encoder_gguf


class _GgufPackage(unittest.TestCase):
    """The tiny GGUF package and the real manager over fake probes (huge budgets: every step pair plans)."""

    @classmethod
    def setUpClass(cls) -> None:
        cls._tmp = tempfile.TemporaryDirectory(prefix="flux3_gguf_")
        cls.addClassCleanup(cls._tmp.cleanup)
        cls.root = Path(cls._tmp.name)
        cls.dit_path, cls.encoder_path = _write_package(cls.root)
        cls.dit = GgufDit("q8_0", cls.dit_path)
        cls.encoder = EncoderSource("q8_0", cls.encoder_path)

    def setUp(self) -> None:
        tiny_ae = patch.object(autoencoder, "build_autoencoder_meta", lambda config=None: _meta_ae())
        tiny_ae.start()
        self.addCleanup(tiny_ae.stop)
        self.probes = FakeProbes()
        self.manager = LargeModelManager(ResidencyArbiter(self.probes, admission_wait_s=0.0), limits_store=MemoryLimitsStore(), tables=_tables(), probes=self.probes, device_class=lambda device: "test_class", host_device=lambda device: False)
        for name, spec in large_model.model_specs().items():
            self.manager.register(spec, model=name)

    def _options(self, variant: Any = WeightFormat.BF16, encoder: EncoderSource = ENCODER_BF16_SOURCE) -> large_model.Flux3ModelOptions:
        options = Flux3Options(model_root=self.root, variant=variant, dit_formats=(WeightFormat.BF16, self.dit), encoder=encoder, encoder_formats=(ENCODER_BF16_SOURCE, self.encoder))
        return large_model.Flux3ModelOptions(options=options, device="cpu", fp8_compute=Fp8Compute.DEQUANT)

    def _run(self, options: large_model.Flux3ModelOptions, steps: Steps, phase: str, fn: Callable[[Any], Any]) -> Any:
        """Run `phase` on its handle: `encode` on the text-encoder handle (the selected encoder format), any other on the pipeline (the selected DiT format)."""
        if phase == large_model.PHASE_ENCODE:
            name, weight_format = large_model.encoder_model_name(EncoderMode.TEXT), options.options.encoder.id
        else:
            name, weight_format = large_model.pipeline_model_name(AeDtype.FLOAT32), options.options.variant.value
        session = self.manager.acquire(name, options, SHAPE, device="cpu", format=weight_format, steps=steps)
        try:
            with session:
                return session.run_phase(phase, fn)
        finally:
            self.manager.unload()


class GgufDitThroughTheManagerTests(_GgufPackage):
    def _denoise(self, ctx: Any) -> torch.Tensor:
        pack = make_pack(tiny_dit_config(), [{"image": 6, "image_cond": 4, "text": 5}], dtype=torch.bfloat16, seed=4)
        with torch.inference_mode():
            return ctx.module("dit")(pack, torch.tensor([0.25]))

    def test_the_gguf_dit_computes_what_its_bf16_twin_does_in_every_step_pair(self) -> None:
        expected = self._run(self._options(), Steps(VramStep.MAX, RamStep.KEEP), "denoise", self._denoise)
        for steps in _ALL_STEPS:
            with self.subTest(steps=steps.label):
                self.assertTrue(torch.equal(self._run(self._options(self.dit), steps, "denoise", self._denoise), expected))

    def test_a_partly_resident_gguf_dit_computes_what_its_twin_does(self) -> None:
        # Huge budgets give whole / parked / streamed DiTs only; a card that holds
        # SOME blocks (found by bisection) makes the hooks juggle the uint8 rows.
        expected = self._run(self._options(), Steps(VramStep.MAX, RamStep.KEEP), "denoise", self._denoise)
        name = large_model.pipeline_model_name(AeDtype.FLOAT32)
        juggled = Steps(VramStep.HALF, RamStep.KEEP)

        def resident_blocks(total: int) -> int | None:
            self.probes.device_total = total
            report = self.manager.profiles(name, self._options(self.dit), SHAPE, device="cpu", format="q8_0", steps=juggled).selected
            plan = getattr(report, "forecast", None)
            line = next((line for line in plan.plan_summary if line.component == "dit"), None) if plan is not None else None
            return None if line is None or line.blocks_host == 0 else line.blocks_device

        low, high = 0, 1 << 40
        while high - low > 1:
            middle = (low + high) // 2
            blocks = resident_blocks(middle)
            low, high = (low, middle) if blocks is None or blocks > 0 else (middle, high)
        self.assertGreater(resident_blocks(high) or 0, 0, "no device size keeps only some DiT blocks resident")
        self.assertTrue(torch.equal(self._run(self._options(self.dit), juggled, "denoise", self._denoise), expected))

    def test_the_quantized_linears_stay_gguf_layers(self) -> None:
        def inspect(ctx: Any) -> dict[str, Any]:
            dit = ctx.module("dit")
            return {name: (type(module).__name__, getattr(module, "weight", None).dtype if hasattr(module, "weight") else None) for name, module in dit.named_modules() if isinstance(module, (GgufLinear, torch.nn.Linear))}

        found = self._run(self._options(self.dit), Steps(VramStep.MAX, RamStep.KEEP), "denoise", inspect)
        gguf = {name for name, (kind, _) in found.items() if kind == "GgufLinear"}
        self.assertTrue(gguf)
        self.assertTrue(all(large_model.dit_block_of(f"{name}.weight") is not None for name in gguf))
        self.assertTrue(all(dtype == torch.uint8 for kind, dtype in found.values() if kind == "GgufLinear"))
        self.assertTrue(all(dtype == torch.bfloat16 for kind, dtype in found.values() if kind == "Linear"))


class GgufEncoderThroughTheManagerTests(_GgufPackage):
    def _encode(self, ctx: Any) -> torch.Tensor:
        ids = torch.randint(0, _ENC_PAD, (1, 37), generator=torch.Generator().manual_seed(7))
        padded, mask = te.right_pad(ids, length=80, pad_id=_ENC_PAD)
        handle = te.TextEncoderHandle(model=ctx.module("encoder"), processor=None, mode=EncoderMode.TEXT, device=torch.device(ctx.exec_device("encoder")), keep_layers=te.ENCODER_KEEP_LAYERS)
        return te.run_text_encoder(handle, te.TokenBatch(input_ids=padded, attention_mask=mask, real_length=37)).tokens

    def test_the_gguf_encoder_encodes_what_its_bf16_twin_does_in_every_step_pair(self) -> None:
        expected = self._run(self._options(), Steps(VramStep.MAX, RamStep.KEEP), "encode", self._encode)
        self.assertEqual(tuple(expected.shape), (80, 8 * _ENC_HIDDEN))
        for steps in _ALL_STEPS:
            with self.subTest(steps=steps.label):
                self.assertTrue(torch.equal(self._run(self._options(encoder=self.encoder), steps, "encode", self._encode), expected))

    def test_the_bridge_fills_a_truncated_gguf_model(self) -> None:
        def inspect(ctx: Any) -> tuple[int, str, str, list[str]]:
            model = ctx.module("encoder")
            meta = [name for name, value in model.state_dict().items() if value.device.type == "meta"]
            return len(model.layers), type(model.embed_tokens).__name__, type(model.layers[0].self_attn.q_proj).__name__, meta

        layers, embed, q_proj, meta = self._run(self._options(encoder=self.encoder), Steps(VramStep.MAX, RamStep.KEEP), "encode", inspect)
        self.assertEqual((layers, embed, q_proj, meta), (te.ENCODER_KEEP_LAYERS, GgufEmbedding.__name__, GgufLinear.__name__, []))

    def test_the_spec_files_and_variant_follow_the_encoder_source(self) -> None:
        spec = large_model.encoder_spec(EncoderMode.TEXT)
        encoder = spec.component("encoder")
        # The files are the PLANNED format's, whichever encoder is selected.
        for options in (self._options(), self._options(encoder=self.encoder)):
            self.assertEqual(list(encoder.files(options, "q8_0")), [self.encoder_path])
            self.assertEqual(len(encoder.files(options, "bf16")), 1)
            self.assertNotEqual(list(encoder.files(options, "bf16")), [self.encoder_path])
        self.assertEqual(spec.phases[0].work(SHAPE, self._options(encoder=self.encoder)).variant, "text_gguf")
        self.assertEqual(spec.phases[0].work(SHAPE, self._options()).variant, "text")
        # GGUF names map through the bridge (layers >= 33 are skipped), shard names through the key map.
        self.assertEqual(encoder.state_keys_of("blk.3.attn_q.weight"), ("layers.3.self_attn.q_proj.weight",))
        self.assertEqual(encoder.state_keys_of("blk.33.attn_q.weight"), ())
        self.assertEqual(encoder.state_keys_of("token_embd.weight"), ("embed_tokens.weight",))
        self.assertEqual(encoder.state_keys_of("model.language_model.norm.weight"), ("norm.weight",))
        self.assertEqual(encoder.device_itemsize("blk.0.attn_q.weight", "U8"), None)
        self.assertEqual(encoder.device_itemsize("blk.0.attn_norm.weight", "F32"), 2)
        vl = large_model.encoder_spec(EncoderMode.VL_REFS).component("encoder")
        with self.assertRaises(ValueError):
            vl.state_keys_of("blk.0.attn_q.weight")


class FormatRowsTests(_GgufPackage):
    def test_every_offered_format_is_a_row_in_offer_order(self) -> None:
        missing = GgufDit("q4_k_m", self.root / "transformer_gguf" / "absent.gguf")
        options = Flux3Options(model_root=self.root, variant=WeightFormat.BF16, dit_formats=(WeightFormat.BF16, WeightFormat.FP8R, self.dit, WeightFormat.NVFP4, missing))
        rows = large_model.formats(large_model.Flux3ModelOptions(options=options, device="cpu", fp8_compute=Fp8Compute.SCALED_MM))
        self.assertEqual(list(rows), ["bf16", "fp8r", "q8_0", "nvfp4", "q4_k_m"])
        self.assertEqual([row.quality_rank for row in rows.values()], [5, 4, 3, 2, 1])
        self.assertEqual({name: (row.available, row.compute_path) for name, row in rows.items()}, {"bf16": (True, "native"), "fp8r": (False, "scaled_mm"), "q8_0": (True, "gguf"), "nvfp4": (False, "dequant"), "q4_k_m": (False, "gguf")})
        self.assertEqual(rows["q8_0"].dit_bytes, self.dit_path.stat().st_size)
        self.assertIn("absent.gguf", rows["q4_k_m"].reason or "")
        self.assertEqual(large_model.compute_path(self.dit, Fp8Compute.SCALED_MM), "gguf")


class ResidencyKeyTests(unittest.TestCase):
    """D9: a GGUF DiT keys the pipeline handle by its file, a GGUF encoder keys the encoder handle by its file; the native / bf16 keys carry neither."""

    ROOT = Path("/fictional/flux3")

    def test_the_keys(self) -> None:
        q8 = GgufDit("q8_0", self.ROOT / "transformer_gguf" / "a.gguf")
        other_file = GgufDit("q8_0", self.ROOT / "transformer_gguf" / "b.gguf")
        enc = EncoderSource("q4_k_m", self.ROOT / "text_encoder_gguf" / "e.gguf")
        base = Flux3Options(model_root=self.ROOT, variant=WeightFormat.NVFP4)
        self.assertEqual(large_model.pipeline_key(base, "nvfp4", "cuda:0"), "flux3_klein:nvfp4|cuda:0|auto|float32|/fictional/flux3")
        with_offer = Flux3Options(model_root=self.ROOT, variant=WeightFormat.NVFP4, dit_formats=(WeightFormat.NVFP4, q8))
        self.assertEqual(large_model.pipeline_key(with_offer, "nvfp4", "cuda:0"), "flux3_klein:nvfp4|cuda:0|auto|float32|/fictional/flux3")
        gguf = Flux3Options(model_root=self.ROOT, variant=q8)
        self.assertEqual(large_model.pipeline_key(gguf, "q8_0", "cuda:0"), f"flux3_klein:q8_0|cuda:0|auto|float32|/fictional/flux3|dit={q8.path}")
        self.assertNotEqual(large_model.pipeline_key(Flux3Options(model_root=self.ROOT, variant=other_file), "q8_0", "cuda:0"), large_model.pipeline_key(gguf, "q8_0", "cuda:0"))
        encoded = Flux3Options(model_root=self.ROOT, variant=WeightFormat.NVFP4, encoder=enc)
        # The encoder never changes the pipeline key; its own key names the format and the file.
        self.assertEqual(large_model.pipeline_key(encoded, "nvfp4", "cuda:0"), large_model.pipeline_key(base, "nvfp4", "cuda:0"))
        self.assertEqual(large_model.encoder_key(encoded, "q4_k_m", "cuda:0"), f"flux3_klein_text_encoder:q4_k_m|cuda:0|text|/fictional/flux3|enc={enc.path}")
        self.assertEqual(large_model.encoder_key(encoded, "bf16", "cuda:0"), "flux3_klein_text_encoder:bf16|cuda:0|text|/fictional/flux3")
        with self.assertRaises(ValueError):
            large_model.pipeline_key(base, "q8_0", "cuda:0")
        with self.assertRaises(ValueError):
            large_model.encoder_key(base, "q4_k_m", "cuda:0")


def _spans(tensors: list[list[Any]]) -> list[TensorSpan]:
    """Header-only `TensorSpan`s of recorded `[name, GGUF dims, type]` rows (the `gguf_stream` span contract, offsets back to back)."""
    plain = {0: ("F32", "float32", 4), 1: ("F16", "float16", 2), 30: ("BF16", "bfloat16", 2)}
    spans: list[TensorSpan] = []
    offset = 0
    for name, dims, type_id in tensors:
        shape = tuple(int(dim) for dim in reversed(dims))
        if type_id in plain:
            token, torch_name, size = plain[type_id]
            nbytes = int(np.prod(shape)) * size
            span = TensorSpan(name, token, torch_name, shape, offset, offset + nbytes)
        else:
            block, type_size = GGML_QUANT_SIZES[GGMLQuantizationType(type_id)]
            row = shape[-1] // block * type_size
            nbytes = int(np.prod(shape[:-1])) * row
            span = TensorSpan(name, "U8", "uint8", (*shape[:-1], row), offset, offset + nbytes, quant_type=type_id)
        spans.append(span)
        offset += nbytes
    return spans


class RecordedHeaderTests(unittest.TestCase):
    """The real files' headers (recorded, no data) pass the checks against the real package configs."""

    @classmethod
    def setUpClass(cls) -> None:
        cls.recorded = json.loads(HEADERS_FILE.read_text(encoding="utf-8"))
        cls._tmp = tempfile.TemporaryDirectory(prefix="flux3_recorded_")
        cls.addClassCleanup(cls._tmp.cleanup)
        root = Path(cls._tmp.name)
        (root / "config.json").write_text(json.dumps(cls.recorded["configs"]["root"]), encoding="utf-8")
        cls.package = read_package_config(root / "config.json")

    def test_the_dit_header_describes_the_dit_of_the_root_config(self) -> None:
        spans = _spans(self.recorded["files"]["dit_Q4_K_M"]["tensors"])
        quant_types = loading.require_gguf_dit(spans, self.package, source=Path("/fictional/dit-Q4_K_M.gguf"), format_id="q4_k_m")
        self.assertEqual(set(quant_types.values()), {int(Q.Q4_K), int(Q.Q6_K)})
        self.assertEqual(len(quant_types), 259)
        # Swapping those layers into a meta DiT works (every key is a linear weight of whole blocks).
        with torch.device("meta"):
            dit = Flux3Dit(self.package.dit)
        from ...runtime.gguf_layers import swap_gguf_modules

        self.assertEqual(swap_gguf_modules(dit, quant_types, compute_dtype=torch.bfloat16), 259)

    def test_the_encoder_header_describes_the_text_half_of_the_package_encoder(self) -> None:
        from ...runtime import qwen3_gguf

        recorded = self.recorded["files"]["encoder_Q4_K_M"]
        spans = _spans(recorded["tensors"])
        text_config = self.recorded["configs"]["text_encoder"]["text_config"]
        self.assertEqual(te.gguf_encoder_mismatches(recorded["metadata"], spans, text_config, keep_layers=te.ENCODER_KEEP_LAYERS), [])
        qwen3_gguf.require_architecture(recorded["metadata"], frozenset({te.GGUF_ENCODER_ARCHITECTURE}), source=Path("/fictional/enc.gguf"), owner="FLUX.3 klein")
        qwen3_gguf.require_known_encoder_names([span.name for span in spans], te.ENCODER_KEEP_LAYERS, source=Path("/fictional/enc.gguf"), owner="FLUX.3 klein", logger=te.log)
        types = qwen3_gguf.gguf_encoder_quant_types(spans, te.ENCODER_KEEP_LAYERS)
        self.assertEqual(types["embed_tokens.weight"], int(Q.Q6_K))
        self.assertNotIn("layers.33.self_attn.q_proj.weight", types)

    def test_every_recorded_file_has_the_counts_of_its_kind(self) -> None:
        for key, recorded in self.recorded["files"].items():
            with self.subTest(file=key):
                self.assertEqual(recorded["tensor_count"], 359 if key.startswith("dit_") else 398)
                self.assertEqual(sum(recorded["type_histogram"].values()), recorded["tensor_count"])


class HeaderRefusalTests(unittest.TestCase):
    """`require_gguf_dit` / `require_gguf_text_encoder` refuse by name before anything loads."""

    @classmethod
    def setUpClass(cls) -> None:
        cls._tmp = tempfile.TemporaryDirectory(prefix="flux3_refusals_")
        cls.addClassCleanup(cls._tmp.cleanup)
        cls.root = Path(cls._tmp.name)
        (cls.root / "config.json").write_text(json.dumps(_config_json(None)), encoding="utf-8")
        cls.package: PackageConfig = read_package_config(cls.root / "config.json")
        cls.dit_tensors, _ = _dit_gguf(_source_model())
        cls.encoder_dir = cls.root / "text_encoder"
        cls.encoder_dir.mkdir()
        config = _encoder_config()
        config.save_pretrained(cls.encoder_dir)
        cls.encoder_tensors, _ = _encoder_gguf(config)

    def _dit_refusal(self, tensors: list[FixtureTensor], needle: str) -> None:
        path = write_gguf(self.root / f"dit-{len(tensors)}-{needle[:8]}.gguf", tensors, architecture="flux")
        with self.assertRaises(ValueError) as caught:
            loading.require_gguf_dit(loading.gguf_dit_layout(path, format_id="q8_0").spans, self.package, source=path, format_id="q8_0")
        message = str(caught.exception)
        self.assertIn("format", message)
        self.assertIn("q8_0", message)
        self.assertIn(needle, message)

    def test_dit_refusals(self) -> None:
        good = self.dit_tensors
        self._dit_refusal([t for t in good if t.name != "ref_time_in.in_layer.weight"], "ref_time_in")
        self._dit_refusal([t for t in good if t.name != "txt_in.weight"], "txt_in.weight")
        self._dit_refusal([*good, FixtureTensor("extra.weight", Q.F32, (4,), np.zeros(4, dtype=np.float32))], "extra.weight")
        wrong_shape = [t if t.name != "final_layer.image.linear.weight" else _encode(t.name, Q.BF16, torch.zeros(t.shape[0], 32))[0] for t in good]
        self._dit_refusal(wrong_shape, "final_layer.image.linear.weight")
        quantized_norm = next(t.name for t in good if t.qtype == Q.F32 and t.shape[0] % 32 == 0)
        bad_norm = [t if t.name != quantized_norm else _encode(t.name, Q.Q8_0, torch.zeros(1, t.shape[0]))[0] for t in good]
        self._dit_refusal(bad_norm, quantized_norm)

    def test_a_safetensors_file_is_not_a_gguf_dit(self) -> None:
        path = self.root / "not.gguf"
        save_file({"x": torch.zeros(2)}, str(path))
        with self.assertRaises(ValueError) as caught:
            loading.gguf_dit_layout(path, format_id="q8_0")
        self.assertIn("format", str(caught.exception))

    def _encoder_refusal(self, needle: str, tensors: list[FixtureTensor] | None = None, *, architecture: str = "qwen3vl", **metadata: Any) -> None:
        path = self.root / f"enc-{architecture}-{needle[:6]}-{len(metadata)}.gguf"
        meta = _encoder_metadata()
        meta = {key.replace("qwen3vl.", f"{architecture}."): value for key, value in meta.items()}
        meta.update(metadata)
        write_gguf(path, tensors if tensors is not None else self.encoder_tensors, architecture=architecture, metadata=meta)
        with self.assertRaises(ValueError) as caught:
            te.require_gguf_text_encoder(path, self.encoder_dir, format_id="q4_k_m")
        message = str(caught.exception)
        self.assertIn("text_encoder_format", message)
        self.assertIn("q4_k_m", message)
        self.assertIn(needle, message)

    def test_encoder_refusals(self) -> None:
        self._encoder_refusal("qwen3", architecture="qwen3")
        self._encoder_refusal("32", **{"qwen3vl.block_count": 32})
        self._encoder_refusal("ширина", **{"qwen3vl.embedding_length": 64})
        self._encoder_refusal("голов ключей", **{"qwen3vl.attention.head_count_kv": 2})
        self._encoder_refusal("layers.5.self_attn.q_proj.weight", [t for t in self.encoder_tensors if t.name != "blk.5.attn_q.weight"])
        foreign = FixtureTensor("v.blk.0.attn_q.weight", Q.F32, (4,), np.zeros(4, dtype=np.float32))
        self._encoder_refusal("v.blk.0.attn_q.weight", [*self.encoder_tensors, foreign])

    def test_the_tiny_encoder_passes_and_a_missing_file_is_named(self) -> None:
        path = write_gguf(self.root / "enc-ok.gguf", self.encoder_tensors, architecture="qwen3vl", metadata=_encoder_metadata())
        te.require_gguf_text_encoder(path, self.encoder_dir, format_id="q8_0")
        with self.assertRaises(FileNotFoundError) as caught:
            te.require_gguf_text_encoder(self.root / "absent.gguf", self.encoder_dir, format_id="q8_0")
        self.assertIn("text_encoder_format", str(caught.exception))


if __name__ == "__main__":
    unittest.main()
