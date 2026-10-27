"""
File: modules/ai_backend/inpaint/flux3_klein/test_large_model.py

Purpose:
Contract tests of `large_model.py`, the FLUX.3 klein `LargeModelSpec`s (the
pipeline handle and the text-encoder handle):
- the pure rules: block names and EXECUTION order of the DiT, device itemsizes,
  the DiT conversion equal to `loading.dit_tensor_cast`, the two residency
  keys, the four registered models (a pipeline per AE dtype, an encoder per
  mode) and the two-handle phase names, the DiT formats (availability,
  quality, compute path) and the encoder formats (one row per offered
  encoder, a GGUF row unavailable in VL_REFS), the forecast inputs (caption /
  reference bounds, per-phase work);
- the spec over a REAL tiny package (tiny `Flux3Dit` in bf16 / fp8r / nvfp4,
  tiny Qwen3-VL encoder, tiny AE) through the real `LargeModelManager` on the
  CPU: the DiT juggled block by block (some blocks resident, the rest parked)
  computes exactly what the fully resident DiT and the standalone
  `loading.load_dit` compute, in every format; the encoder loaded by the manager
  (its own handle) equals the standalone `load_text_encoder_model`, in both
  modes.

Notes:
CPU only; files under a temporary directory removed by each class. The tiny
checkpoints come from `test_loading.py` / `test_text_encoder.py` helpers; the
AE meta build is patched to a tiny `AeConfig` (the released AE is 84 M
parameters) on its defining module.
"""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from typing import Any
from unittest.mock import patch

import torch
from safetensors.torch import save_file

from ...runtime.large_models.manager import LargeModelManager, MemoryLimitsStore
from ...runtime.large_models.planner import Planned
from ...runtime.large_models.spec import ExecDevice, RequestShape
from ...runtime.large_models.tiers import RamStep, Steps, Tier, VramStep
from ...runtime.residency.arbiter import ResidencyArbiter
from . import autoencoder, large_model, loading, packing, quant
from . import text_encoder as te
from ._service_test_fixtures import FakeProbes
from .config import ENCODER_BF16_SOURCE, EncoderSource, WeightFormat, read_package_config, resolve_model_files
from .dit import Flux3Dit
from .params import AeDtype, EncoderMode, Flux3Options, Fp8Compute
from .test_dit import make_pack, tiny_dit_config
from .test_loading import _config_json, _fp8r_checkpoint, _nvfp4_checkpoint, _source_model
from .test_text_encoder import _full_model, _tiny_config, _write_checkpoint

_TINY_AE = autoencoder.AeConfig(embed_dim=4, ch=32, ch_mult=(1, 1), num_res_blocks=1)


def _options(root: Path, **overrides: Any) -> Flux3Options:
    return Flux3Options(model_root=root, variant=overrides.pop("variant", WeightFormat.BF16), **overrides)


def _model_options(root: Path, fp8: Fp8Compute = Fp8Compute.DEQUANT, **overrides: Any) -> large_model.Flux3ModelOptions:
    return large_model.Flux3ModelOptions(options=_options(root, **overrides), device="cpu", fp8_compute=fp8)


class RuleTests(unittest.TestCase):
    def test_dit_block_names(self) -> None:
        cases = {
            "content_mode_blocks.image.3.q_proj.weight": "content_mode_blocks.image.3",
            "content_mode_blocks.image_cond.0.mlp_in.weight_scale": "content_mode_blocks.image_cond.0",
            "txt_mode_blocks.4.norm.query_norm.scale": "txt_mode_blocks.4",
            "single_blocks.27.attn_out.weight": "single_blocks.27",
            "txt_in.weight": None,
            "single_stream_modulations.image.lin.weight": None,
            "final_layer.image.linear.weight": None,
        }
        for key, block in cases.items():
            with self.subTest(key=key):
                self.assertEqual(large_model.dit_block_of(key), block)

    def test_the_dit_block_order_is_the_forward_order(self) -> None:
        # Every block of a real (meta) DiT, in the order Flux3Dit.forward runs them.
        with torch.device("meta"):
            dit = Flux3Dit(tiny_dit_config())
        names = {large_model.dit_block_of(key) for key in dit.state_dict()} - {None}
        pack = make_pack(tiny_dit_config(), [{"image": 4, "image_cond": 2, "text": 3}], dtype=torch.float32, seed=1)
        torch.manual_seed(0)
        real = Flux3Dit(tiny_dit_config()).eval()
        called: list[str] = []
        for name in names:
            real.get_submodule(name).register_forward_pre_hook(lambda _module, _args, name=name: called.append(name))
        with torch.inference_mode():
            real(pack, torch.tensor([0.25]))
        self.assertEqual(large_model.dit_block_order(sorted(names)), called)
        from . import dit as dit_module

        self.assertEqual(large_model._DIT_CONTENT_MODES, dit_module._CONTENT_MODES)

    def test_device_itemsize(self) -> None:
        rule = large_model.dit_device_itemsize
        self.assertEqual(rule("single_blocks.0.q_proj.weight", "F32"), 2)
        self.assertEqual(rule("txt_in.weight", "BF16"), 2)
        self.assertIsNone(rule("single_blocks.0.q_proj.weight", "F8_E4M3"))
        self.assertIsNone(rule("single_blocks.0.q_proj.weight", "U8"))
        self.assertIsNone(rule("single_blocks.0.q_proj.weight_scale", "F32"))
        self.assertIsNone(rule("single_blocks.0.q_proj.weight_scale_2", "F32"))
        self.assertIsNone(rule("single_blocks.0.q_proj.input_scale", "F32"))
        self.assertIsNone(rule("some.index", "I64"))

    def test_the_rule_conversion_equals_the_cast_of_the_standalone_loader(self) -> None:
        source = _source_model()
        for name, (state, _reference) in (("bf16", (source.state_dict(), None)), ("fp8r", _fp8r_checkpoint(source)), ("nvfp4", _nvfp4_checkpoint(source))):
            with torch.device("meta"):
                dit = Flux3Dit(tiny_dit_config())
            quant.swap_quantized_linears(dit, state.keys())
            cast = loading.dit_tensor_cast(loading._quantized_buffer_dtypes(dit))
            for key, tensor in state.items():
                with self.subTest(format=name, key=key):
                    ((converted_key, converted),) = list(loading.convert_dit_tensor(key, tensor))
                    expected = cast(key, tensor)
                    self.assertEqual((converted_key, converted.dtype), (key, expected.dtype))

    def test_the_residency_keys(self) -> None:
        root = Path("/fictional/flux3")
        base = _options(root)
        key = large_model.pipeline_key(base, "nvfp4", "cuda:0")
        self.assertEqual(key, "flux3_klein:nvfp4|cuda:0|auto|float32|/fictional/flux3")
        # The pipeline key ignores everything of the encoder handle and the request.
        same = (
            _options(root, reference_max_pixels=128 * 128),
            _options(root, vram_step=VramStep.MIN, ram_step=RamStep.RUN),
            _options(root, device="cuda:1"),
            _options(root, encoder_mode=EncoderMode.VL_REFS),
            _options(root, encoder_on_cpu=True),
        )
        for options in same:
            self.assertEqual(large_model.pipeline_key(options, "nvfp4", "cuda:0"), key)
        different = (
            (_options(root, ae_dtype=AeDtype.BFLOAT16), "nvfp4", "cuda:0"),
            (_options(root, fp8_compute=Fp8Compute.DEQUANT), "nvfp4", "cuda:0"),
            (_options(Path("/fictional/other")), "nvfp4", "cuda:0"),
            (base, "fp8r", "cuda:0"),
            (base, "nvfp4", "cuda:1"),
        )
        for options, weight_format, device in different:
            self.assertNotEqual(large_model.pipeline_key(options, weight_format, device), key)
        encoder = large_model.encoder_key(base, "bf16", "cuda:0")
        self.assertEqual(encoder, "flux3_klein_text_encoder:bf16|cuda:0|text|/fictional/flux3")
        # The encoder key ignores the pipeline's choices; the host pin is the manager's suffix.
        for options in (_options(root, ae_dtype=AeDtype.BFLOAT16), _options(root, variant=WeightFormat.FP8R), _options(root, fp8_compute=Fp8Compute.DEQUANT)):
            self.assertEqual(large_model.encoder_key(options, "bf16", "cuda:0"), encoder)
        self.assertEqual(large_model.encoder_key(_options(root, encoder_mode=EncoderMode.VL_REFS), "bf16", "cuda:0"), "flux3_klein_text_encoder:bf16|cuda:0|vl_refs|/fictional/flux3")
        pinned = _model_options(root, encoder_on_cpu=True)
        self.assertEqual(large_model.encoder_spec(EncoderMode.TEXT).key(pinned, "bf16", "cuda:0"), f"{encoder}|host_pin=encoder")
        self.assertEqual(large_model.pipeline_spec(AeDtype.FLOAT32).key(pinned, "nvfp4", "cuda:0"), key)
        with self.assertRaises(ValueError):
            large_model.encoder_key(base, "q8_0", "cuda:0")

    def test_two_pipeline_models_and_two_encoder_models(self) -> None:
        specs = large_model.model_specs()
        pipelines = {large_model.pipeline_model_name(dtype) for dtype in AeDtype}
        encoders = {large_model.encoder_model_name(mode) for mode in EncoderMode}
        self.assertEqual(pipelines, {"flux3_klein.ae_float32", "flux3_klein.ae_bfloat16"})
        self.assertEqual(encoders, {"flux3_klein_text_encoder.text", "flux3_klein_text_encoder.vl_refs"})
        self.assertEqual(set(specs), pipelines | encoders)
        for name, spec in specs.items():
            # One calibration family for both handles: the encode entries keep matching.
            self.assertEqual(spec.family, large_model.FAMILY)
            self.assertEqual(spec.fallbacks, ())
            if name in pipelines:
                self.assertEqual([phase.name for phase in spec.phases], ["ref_encode", "denoise", "decode"])
                self.assertEqual([component.name for component in spec.components], ["dit", "ae"])
                self.assertEqual(spec.component("dit").exec_devices_allowed, frozenset({ExecDevice.RUN_DEVICE}))
                self.assertEqual(spec.host_pin(_model_options(Path("/fictional/flux3"), encoder_on_cpu=True)), frozenset())
            else:
                self.assertEqual([phase.name for phase in spec.phases], ["encode"])
                self.assertEqual(spec.component("encoder").exec_devices_allowed, frozenset({ExecDevice.RUN_DEVICE, ExecDevice.CPU}))
                self.assertEqual(spec.host_pin(_model_options(Path("/fictional/flux3"), encoder_on_cpu=True)), frozenset({"encoder"}))
        pair = large_model.FLUX3_PAIR
        self.assertEqual((pair.pipeline, pair.rest, pair.encode), (("ref_encode", "denoise", "decode"), "ref_encode", "encode"))

    def test_compute_paths(self) -> None:
        self.assertEqual(large_model.compute_path(WeightFormat.BF16, Fp8Compute.SCALED_MM), "native")
        self.assertEqual(large_model.compute_path(WeightFormat.NVFP4, Fp8Compute.SCALED_MM), "dequant")
        self.assertEqual(large_model.compute_path(WeightFormat.FP8R, Fp8Compute.SCALED_MM), "scaled_mm")
        self.assertEqual(large_model.compute_path(WeightFormat.FP8R, Fp8Compute.DEQUANT), "dequant")
        with self.assertRaises(ValueError):
            large_model.Flux3ModelOptions(options=_options(Path("/fictional/flux3")), device="cpu", fp8_compute=Fp8Compute.AUTO)


class ShapeTests(unittest.TestCase):
    def test_the_caption_bound(self) -> None:
        self.assertGreaterEqual(large_model.caption_token_bound("кот"), len("кот".encode("utf-8")))
        self.assertEqual(large_model.caption_token_bound("x" * 10000), 4096)

    def test_the_reference_bound_is_idempotent_on_prepared_sizes_and_bounds_raw_ones(self) -> None:
        from PIL import Image

        for sizes, cap in (([(1200, 800)], None), ([(1200, 800)], 512 * 512), ([(40, 64)], None), ([(3000, 2000), (640, 480)], None), ([(3000, 2000), (640, 480)], 256 * 256)):
            with self.subTest(sizes=sizes, cap=cap):
                prepared = packing.prepare_references([Image.new("RGB", size) for size in sizes], None, None, max_pixels=cap).images
                for raw, image in zip(sizes, prepared):
                    pixels = image.width * image.height
                    self.assertEqual(large_model.reference_pixel_bound(image.width, image.height, count=len(sizes), max_pixels=cap), pixels)
                    self.assertGreaterEqual(large_model.reference_pixel_bound(*raw, count=len(sizes), max_pixels=cap), pixels)

    def test_phase_work(self) -> None:
        root = Path("/fictional/flux3")
        shape = large_model.request_shape(1024, 768, [(512, 512)], caption="a cat", steps=8)

        def works(mode: EncoderMode, ae_dtype: AeDtype, options: large_model.Flux3ModelOptions) -> dict[str, Any]:
            phases = (*large_model.encoder_spec(mode).phases, *large_model.pipeline_spec(ae_dtype).phases)
            return {phase.name: phase.work(shape, options) for phase in phases}

        work = works(EncoderMode.TEXT, AeDtype.FLOAT32, _model_options(root))
        text = 80  # the 37-token bound pads to 80
        self.assertEqual((work["encode"].tokens, work["encode"].variant), (text, "text"))
        self.assertEqual((work["ref_encode"].pixels, work["ref_encode"].variant), (512 * 512, "float32"))
        self.assertEqual((work["denoise"].tokens, work["denoise"].passes), (64 * 48 + 32 * 32 + text, 8))
        self.assertEqual((work["decode"].pixels, work["decode"].variant), (1024 * 768, "float32"))
        options = _model_options(root, encoder_mode=EncoderMode.VL_REFS, ae_dtype=AeDtype.BFLOAT16, vl_ref_max_pixels=64 * 64)
        work = works(EncoderMode.VL_REFS, AeDtype.BFLOAT16, options)
        # 37 caption tokens + 4096 / 1024 image tokens + 2 wrappers = 43 -> 80.
        self.assertEqual((work["encode"].tokens, work["encode"].variant), (80, "vl"))
        self.assertEqual(work["decode"].variant, "bfloat16")
        unknown = RequestShape(width=64, height=64)
        self.assertEqual(large_model.encoder_spec(EncoderMode.TEXT).phases[0].work(unknown, _model_options(root)).tokens, 560)  # 512 -> padded


def _write_package(root: Path) -> None:
    """A real tiny package: the tiny DiT in all three formats, a tiny Qwen3-VL encoder and a tiny AE."""
    source = _source_model()
    for variant, state in ((WeightFormat.BF16, source.state_dict()), (WeightFormat.FP8R, _fp8r_checkpoint(source)[0]), (WeightFormat.NVFP4, _nvfp4_checkpoint(source)[0])):
        directory = root if variant is WeightFormat.BF16 else root / "variants" / variant.value
        directory.mkdir(parents=True, exist_ok=True)
        save_file({key: value.contiguous() for key, value in state.items()}, str(directory / "model.safetensors"))
        (directory / "config.json").write_text(json.dumps(_config_json(None if variant is WeightFormat.BF16 else variant.value)), encoding="utf-8")
    torch.manual_seed(3)
    ae = autoencoder.AutoEncoder(_TINY_AE)
    save_file({key: value.contiguous() for key, value in ae.state_dict().items()}, str(root / "ae.safetensors"))
    encoder_dir = root / "text_encoder"
    encoder_dir.mkdir()
    config = _tiny_config()
    _write_checkpoint(encoder_dir, config, _full_model(config))


class _Package(unittest.TestCase):
    """The tiny package, the real manager over fake probes and the four FLUX.3 models registered."""

    @classmethod
    def setUpClass(cls) -> None:
        cls._tmp = tempfile.TemporaryDirectory(prefix="flux3_spec_")
        cls.addClassCleanup(cls._tmp.cleanup)
        cls.root = Path(cls._tmp.name)
        _write_package(cls.root)

    def setUp(self) -> None:
        tiny_ae = patch.object(autoencoder, "build_autoencoder_meta", lambda config=None: _meta_ae())
        tiny_ae.start()
        self.addCleanup(tiny_ae.stop)
        self.probes = FakeProbes()
        # The CPU stands in for a GPU here (fake device probes): plan it as a device, not as the host.
        self.manager = LargeModelManager(ResidencyArbiter(self.probes, admission_wait_s=0.0), limits_store=MemoryLimitsStore(), tables=_tables(), probes=self.probes, device_class=lambda device: "test_class", host_device=lambda device: False)
        for name, spec in large_model.model_specs().items():
            self.manager.register(spec, model=name)

    def _acquire(self, weight_format: WeightFormat, steps: Steps, **overrides: Any) -> Any:
        """A pipeline session of `weight_format`."""
        options = _model_options(self.root, variant=weight_format, **overrides)
        name = large_model.pipeline_model_name(options.options.ae_dtype)
        return self.manager.acquire(name, options, SHAPE, device="cpu", format=weight_format.value, steps=steps)

    def _acquire_encoder(self, steps: Steps, **overrides: Any) -> Any:
        """A text-encoder session of the selected encoder format (bf16 unless overridden)."""
        options = _model_options(self.root, **overrides)
        name = large_model.encoder_model_name(options.options.encoder_mode)
        return self.manager.acquire(name, options, SHAPE, device="cpu", format=options.options.encoder.id, steps=steps)


def _tables() -> Any:
    """Measured round-number costs where the DENOISE is the peak phase (1 GiB activation, every other phase none), so a tight card juggles the DiT."""
    from ...runtime.large_models.cost_tables import parse_tables

    def entry(phase: str, exec_kind: str, fixed: int, family: str = large_model.FAMILY) -> dict[str, Any]:
        return {"family": family, "phase": phase, "variant": "*", "format": "*", "compute_path": "*", "exec": exec_kind, "measured": True, "fixed_bytes": fixed, "bytes_per_token": 0, "bytes_per_pixel": 0, "reserved_factor": 1.0, "seconds_fixed": 0.01, "seconds_per_ktoken_pass": 0.0, "seconds_per_ktoken2_pass": 0.0, "seconds_per_mpixel": 0.0}

    rate = 1 << 30
    return parse_tables(
        {
            "device_class": "test",
            "calibrated": True,
            "source": "test",
            "device": {"runtime_overhead_bytes": 0, "margin_fraction": 0.0, "margin_cap_bytes": 0, "park_bytes_per_s": rate, "restore_bytes_per_s": rate, "block_h2d_bytes_per_s": rate, "disk_cold_bytes_per_s": rate, "disk_warm_bytes_per_s": rate, "prefetch_depth": 1},
            "phases": [entry("denoise", "device", 1 << 30), entry("encode", "device", 0), entry("encode", "cpu", 0), entry("ref_encode", "device", 0), entry("decode", "device", 0), entry("*", "device", 0, family="*"), entry("*", "cpu", 0, family="*")],
        },
        source="test",
    )


def _meta_ae() -> autoencoder.AutoEncoder:
    with torch.device("meta"):
        return autoencoder.AutoEncoder(_TINY_AE)


SHAPE = RequestShape(width=64, height=64, prompt_tokens=16, steps=2)

#: Everything on the device / the DiT juggled from RAM (the steps the two DiT runs use).
_RESIDENT = Steps(VramStep.MAX, RamStep.KEEP)
_JUGGLED = Steps(VramStep.HALF, RamStep.KEEP)


class ManagerDitTests(_Package):
    def _pack(self) -> Any:
        return make_pack(tiny_dit_config(), [{"image": 6, "image_cond": 4, "text": 5}], dtype=torch.bfloat16, seed=4)

    def _denoise(self, session: Any) -> torch.Tensor:
        pack = self._pack()

        def fn(ctx: Any) -> torch.Tensor:
            with torch.inference_mode():
                return ctx.module("dit")(pack, torch.tensor([0.25]))

        with session:
            return session.run_phase("denoise", fn)

    def _juggling_room(self, weight_format: WeightFormat) -> int:
        """A device size on which (HALF, KEEP) keeps some DiT blocks resident and parks the rest (binary searches)."""
        name = large_model.pipeline_model_name(AeDtype.FLOAT32)
        options = _model_options(self.root, variant=weight_format)

        def dit_line(total: int) -> Any:
            self.probes.device_total = total
            result = self.manager.profiles(name, options, SHAPE, device="cpu", format=weight_format.value, steps=_JUGGLED).selected
            if not isinstance(result, Planned):
                return None
            return next(line for line in result.forecast.plan_summary if line.component == "dit")

        def smallest(predicate: Any) -> int:
            low, high = 0, 1 << 40
            while high - low > 1:
                middle = (low + high) // 2
                low, high = (low, middle) if predicate(dit_line(middle)) else (middle, high)
            return high

        fits = smallest(lambda line: line is not None and line.blocks_device > 0)
        whole = smallest(lambda line: line is not None and line.blocks_host == 0)
        total = (fits + whole) // 2
        line = dit_line(total)
        self.assertTrue(line is not None and line.blocks_device > 0 and line.blocks_host > 0, (fits, whole, line))
        return total

    def test_a_juggled_dit_computes_what_the_resident_one_does_in_every_format(self) -> None:
        for weight_format in WeightFormat:
            with self.subTest(format=weight_format):
                files = resolve_model_files(self.root, weight_format)
                standalone = loading.load_dit(files, read_package_config(files.dit_config), variant=weight_format, device="cpu", fp8_compute=Fp8Compute.DEQUANT)
                with torch.inference_mode():
                    expected = standalone.dit(self._pack(), torch.tensor([0.25]))
                self.probes.device_total = 1 << 44
                resident = self._denoise(self._acquire(weight_format, _RESIDENT))
                self.manager.unload()
                self.probes.device_total = self._juggling_room(weight_format)
                session = self._acquire(weight_format, _JUGGLED)
                plan = session.plan.phase("denoise").components["dit"]
                self.assertEqual(plan.tier, Tier.HOST)
                self.assertGreater(plan.resident_blocks, 0)
                juggled = self._denoise(session)
                self.manager.unload()
                self.assertTrue(torch.equal(resident, expected))
                self.assertTrue(torch.equal(juggled, expected))


class ManagerEncoderTests(_Package):
    def test_the_manager_loads_the_encoder_the_standalone_loader_loads(self) -> None:
        for mode in EncoderMode:
            with self.subTest(mode=mode):
                reference, _report = te.load_text_encoder_model(self.root / "text_encoder", mode=mode, device="cpu", dtype=torch.bfloat16)
                session = self._acquire_encoder(_RESIDENT, encoder_mode=mode)
                with session:
                    state = session.run_phase("encode", lambda ctx: {key: value.clone() for key, value in ctx.module("encoder").state_dict().items()})
                self.manager.unload()
                expected = reference.state_dict()
                self.assertEqual(set(state), set(expected))
                for key, value in expected.items():
                    self.assertTrue(torch.equal(state[key], value), key)

    def test_formats_report_every_variant(self) -> None:
        rows = large_model.formats(_model_options(self.root, fp8=Fp8Compute.SCALED_MM))
        self.assertEqual({name: (info.available, info.compute_path, info.quality_rank) for name, info in rows.items()}, {"bf16": (True, "native", 3), "fp8r": (True, "scaled_mm", 2), "nvfp4": (True, "dequant", 1)})
        self.assertTrue(all(info.dit_bytes > 0 for info in rows.values()))
        self.assertGreater(rows["bf16"].dit_bytes, rows["nvfp4"].dit_bytes)

    def test_encoder_formats_report_every_offered_encoder(self) -> None:
        gguf = EncoderSource("q8_0", self.root / "text_encoder_gguf" / "enc-Q8_0.gguf")
        gguf.path.parent.mkdir(exist_ok=True)
        gguf.path.write_bytes(b"x" * 9)
        self.addCleanup(gguf.path.unlink)
        absent = EncoderSource("q4_k_m", self.root / "text_encoder_gguf" / "absent.gguf")
        offered = (ENCODER_BF16_SOURCE, gguf, absent)
        rows = large_model.encoder_formats(_model_options(self.root, encoder_formats=offered))
        self.assertEqual(list(rows), ["bf16", "q8_0", "q4_k_m"])
        self.assertEqual({name: (info.available, info.compute_path, info.quality_rank) for name, info in rows.items()}, {"bf16": (True, "native", 3), "q8_0": (True, "gguf", 2), "q4_k_m": (False, "gguf", 1)})
        self.assertEqual(rows["q8_0"].dit_bytes, 9)
        self.assertGreater(rows["bf16"].dit_bytes, 0)
        self.assertIn("absent.gguf", rows["q4_k_m"].reason)
        # VL_REFS: a GGUF holds no vision tower, so its row is unavailable with the D6 reason.
        vl = large_model.encoder_formats(_model_options(self.root, encoder_mode=EncoderMode.VL_REFS, encoder_formats=offered))
        self.assertTrue(vl["bf16"].available)
        self.assertFalse(vl["q8_0"].available)
        self.assertIn("GGUF", vl["q8_0"].reason)
        self.assertEqual(vl["q8_0"].dit_bytes, 0)
        # Default: bf16 alone, plus a selected GGUF encoder.
        self.assertEqual(list(large_model.encoder_formats(_model_options(self.root))), ["bf16"])
        self.assertEqual(list(large_model.encoder_formats(_model_options(self.root, encoder=gguf))), ["bf16", "q8_0"])

    def test_the_encoder_files_follow_the_planned_format(self) -> None:
        gguf = EncoderSource("q8_0", self.root / "enc.gguf")
        options = _model_options(self.root, encoder_formats=(ENCODER_BF16_SOURCE, gguf))
        component = large_model.encoder_spec(EncoderMode.TEXT).component("encoder")
        self.assertTrue(all(path.parent.name == "text_encoder" for path in component.files(options, "bf16")))
        self.assertIs(large_model.encoder_source(options.options, "q8_0"), gguf)
        with self.assertRaises(FileNotFoundError):
            component.files(options, "q8_0")
        with self.assertRaises(ValueError):
            component.files(options, "q4_k_m")

    def test_a_missing_format_is_unavailable_with_its_reason(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            rows = large_model.formats(_model_options(Path(directory)))
        self.assertFalse(any(info.available for info in rows.values()))
        self.assertTrue(all(info.reason and info.dit_bytes == 0 for info in rows.values()))


if __name__ == "__main__":
    unittest.main()
