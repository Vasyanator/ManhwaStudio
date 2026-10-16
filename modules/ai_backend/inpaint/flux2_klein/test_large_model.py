"""
File: modules/ai_backend/inpaint/flux2_klein/test_large_model.py

Purpose:
Unit tests for `large_model.py`: the FLUX.2 specs as the large-model manager
sees them — model names and registration, the transformer's key mapping and
block structure, the encoder's truncation / lm_head drop / fp8 split, the
request options and shape, each phase's work, the decode fallbacks, the
pre-load refusals, and a real load + forward of the tiny real tree through the
manager on the CPU.

Notes:
The key-mapping tests need diffusers (its converter is the mapping's owner);
the real-load test needs torch, diffusers and transformers. Each skips
cleanly without them. No GPU, no network; files go to temporary directories.
"""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from modules.ai_backend.inpaint import flux2_klein as svc
from modules.ai_backend.inpaint.flux2_klein import attention, large_model
from modules.ai_backend.runtime.large_models.manager import LargeModelManager, MemoryLimitsStore
from modules.ai_backend.runtime.large_models.spec import ExecDevice, Role
from modules.ai_backend.runtime.residency.arbiter import ResidencyArbiter

from ._test_fixtures import _RoomyProbes, _TempTreeCase, _flat_tables, _write_header_only, _write_safetensors


def _has(*modules: str) -> bool:
    try:
        for name in modules:
            __import__(name)
    except ImportError:
        return False
    return True


class NamesAndRegistrationTests(unittest.TestCase):
    def test_model_names(self) -> None:
        self.assertEqual(large_model.pipeline_model_name("bfloat16"), "flux2_klein")
        self.assertEqual(large_model.pipeline_model_name("float16"), "flux2_klein.float16")
        self.assertEqual(large_model.encoder_model_name(False), "flux2_klein_text_encoder")
        self.assertEqual(large_model.encoder_model_name(True), "flux2_klein_text_encoder.fp8")
        self.assertEqual(large_model.encoder_model_name(False, cpu_only=True), "flux2_klein_text_encoder.cpu")
        self.assertEqual(large_model.encoder_model_name(True, cpu_only=True), "flux2_klein_text_encoder.fp8.cpu")

    def test_registration_is_idempotent(self) -> None:
        manager = LargeModelManager(ResidencyArbiter(_RoomyProbes()), limits_store=MemoryLimitsStore(), tables=_flat_tables(), probes=_RoomyProbes())
        large_model.register_specs(manager)
        large_model.register_specs(manager)
        self.assertEqual(sorted(manager.models()), sorted(large_model.pipeline_model_names() + large_model.encoder_model_names()))

    def test_the_spec_shapes(self) -> None:
        pipeline = large_model.pipeline_spec("bfloat16")
        self.assertEqual([component.name for component in pipeline.components], ["transformer", "vae"])
        self.assertEqual([component.role for component in pipeline.components], [Role.DIT, Role.AE])
        self.assertEqual([phase.name for phase in pipeline.phases], ["denoise", "decode"])
        self.assertEqual(pipeline.phases[0].uses, frozenset({"transformer", "vae"}), "the pipeline call VAE-encodes the region")
        self.assertEqual([fallback.name for fallback in pipeline.fallbacks], ["vae_tiling", "vae_tile_lowered"])
        encoder = large_model.encoder_spec(False)
        self.assertEqual(encoder.components[0].exec_devices_allowed, frozenset({ExecDevice.RUN_DEVICE, ExecDevice.CPU}))
        self.assertEqual([phase.name for phase in encoder.phases], ["encode"])
        self.assertEqual(large_model.encoder_spec(False, cpu_only=True).components[0].exec_devices_allowed, frozenset({ExecDevice.CPU}), "the legacy wire's encoder")


class TransformerKeyTests(unittest.TestCase):
    def test_block_of_and_order(self) -> None:
        self.assertEqual(large_model.transformer_block_of("transformer_blocks.3.attn.to_q.weight"), "transformer_blocks.3")
        self.assertEqual(large_model.transformer_block_of("single_transformer_blocks.12.attn.to_out.weight"), "single_transformer_blocks.12")
        self.assertIsNone(large_model.transformer_block_of("x_embedder.weight"))
        names = ["single_transformer_blocks.10", "transformer_blocks.1", "single_transformer_blocks.2", "transformer_blocks.0"]
        self.assertEqual(large_model.transformer_block_order(names), ["transformer_blocks.0", "transformer_blocks.1", "single_transformer_blocks.2", "single_transformer_blocks.10"])

    def test_diffusers_keys_are_identity(self) -> None:
        self.assertEqual(large_model.transformer_state_keys("transformer_blocks.0.attn.to_q.weight"), ("transformer_blocks.0.attn.to_q.weight",))
        self.assertEqual(large_model.transformer_state_keys("double_stream_modulation_img.linear.weight"), ("double_stream_modulation_img.linear.weight",))

    @unittest.skipUnless(_has("torch", "diffusers"), "needs torch and diffusers")
    def test_bfl_keys_go_through_the_diffusers_converter(self) -> None:
        self.assertEqual(
            large_model.transformer_state_keys("double_blocks.0.img_attn.qkv.weight"),
            ("transformer_blocks.0.attn.to_q.weight", "transformer_blocks.0.attn.to_k.weight", "transformer_blocks.0.attn.to_v.weight"),
        )
        self.assertEqual(large_model.transformer_state_keys("single_blocks.5.linear1.weight"), ("single_transformer_blocks.5.attn.to_qkv_mlp_proj.weight",))
        self.assertEqual(large_model.transformer_state_keys("double_stream_modulation_img.lin.weight"), ("double_stream_modulation_img.linear.weight",))
        self.assertEqual(large_model.transformer_state_keys("final_layer.linear.weight"), ("proj_out.weight",))

    @unittest.skipUnless(_has("torch", "diffusers"), "needs torch and diffusers")
    def test_the_conversion_splits_values_and_casts(self) -> None:
        import torch

        spec = large_model.pipeline_spec("float16")
        convert = spec.component("transformer").convert
        assert convert is not None
        fused = torch.arange(6 * 2, dtype=torch.bfloat16).reshape(6, 2)
        produced = dict(convert("double_blocks.0.img_attn.qkv.weight", fused))
        self.assertEqual(sorted(produced), ["transformer_blocks.0.attn.to_k.weight", "transformer_blocks.0.attn.to_q.weight", "transformer_blocks.0.attn.to_v.weight"])
        self.assertTrue(all(value.dtype == torch.float16 for value in produced.values()))
        self.assertTrue(torch.equal(produced["transformer_blocks.0.attn.to_k.weight"].float(), fused[2:4].float()))


class EncoderKeyTests(unittest.TestCase):
    def test_truncation_and_the_head_are_dropped(self) -> None:
        keys = large_model.encoder_spec(False).component("text_encoder").state_keys_of
        self.assertEqual(keys("lm_head.weight"), ())
        self.assertEqual(keys(f"model.layers.{svc.ENCODER_KEEP_LAYERS}.mlp.up_proj.weight"), ())
        self.assertEqual(keys(f"model.layers.{svc.ENCODER_KEEP_LAYERS - 1}.mlp.up_proj.weight"), (f"layers.{svc.ENCODER_KEEP_LAYERS - 1}.mlp.up_proj.weight",))
        self.assertEqual(keys("model.embed_tokens.weight"), ("embed_tokens.weight",))
        self.assertEqual(keys("layers.0.input_layernorm.weight"), ("layers.0.input_layernorm.weight",), "a Qwen3Model checkpoint has no prefix")

    def test_fp8_splits_the_linear_weights_only(self) -> None:
        component = large_model.encoder_spec(True).component("text_encoder")
        self.assertEqual(component.state_keys_of("model.layers.2.self_attn.q_proj.weight"), ("layers.2.self_attn.q_proj.weight_fp8", "layers.2.self_attn.q_proj.weight_scale"))
        self.assertEqual(component.state_keys_of("model.layers.2.input_layernorm.weight"), ("layers.2.input_layernorm.weight",))
        self.assertEqual(component.device_itemsize("model.layers.2.mlp.down_proj.weight", "F16"), 1)
        self.assertEqual(component.device_itemsize("model.embed_tokens.weight", "F16"), 2)

    def test_block_of(self) -> None:
        self.assertEqual(large_model.encoder_block_of("layers.7.mlp.up_proj.weight"), "layers.7")
        self.assertIsNone(large_model.encoder_block_of("norm.weight"))

    @unittest.skipUnless(_has("torch"), "needs torch")
    def test_the_fp8_conversion_round_trips_within_the_documented_error(self) -> None:
        import torch

        if getattr(torch, "float8_e4m3fn", None) is None:
            self.skipTest("no float8 in this torch")
        convert = large_model.encoder_spec(True).component("text_encoder").convert
        assert convert is not None
        weight = torch.randn(16, 32, generator=torch.Generator().manual_seed(0)).to(torch.float16)
        produced = dict(convert("model.layers.0.mlp.up_proj.weight", weight))
        quantized, scale = produced["layers.0.mlp.up_proj.weight_fp8"], produced["layers.0.mlp.up_proj.weight_scale"]
        self.assertEqual((quantized.dtype, scale.shape), (torch.float8_e4m3fn, (16, 1)))
        restored = quantized.to(torch.bfloat16).float() * scale.float()
        relative = ((restored - weight.float()).abs().amax() / weight.float().abs().amax()).item()
        self.assertLess(relative, 0.08)


class OptionsAndWorkTests(_TempTreeCase):
    def _options(self, **overrides: object) -> large_model.PipelineOptions:
        normalized = svc.normalize_flux2_klein_params(self.params(**overrides))
        return large_model.pipeline_options(normalized, guidance=False)

    def test_options_come_from_the_request_and_the_vae_config(self) -> None:
        options = self._options(steps=4, strength=1.0)
        self.assertEqual((options.steps, options.vae_tile_px, options.vae_scale), (4, 1024, 8))
        self.assertFalse(options.vae_tile_lowered)

    def test_denoise_work_counts_text_noisy_clean_and_reference_tokens(self) -> None:
        spec = large_model.pipeline_spec("bfloat16")
        work = spec.phases[0].work
        options = self._options()
        plain = work(large_model.request_shape(512, 512, with_reference=False, max_sequence_length=512, steps=4), options)
        self.assertEqual((plain.tokens, plain.passes, plain.extra_device_bytes), (512 + 2 * 1024, 4, 0))
        marked = work(large_model.request_shape(512, 512, with_reference=True, max_sequence_length=512, steps=4), options)
        self.assertEqual(marked.tokens, 512 + 3 * 1024)
        guided = work(large_model.request_shape(512, 512, with_reference=False, max_sequence_length=512, steps=4), large_model.pipeline_options(svc.normalize_flux2_klein_params(self.params()), guidance=True))
        self.assertEqual(guided.passes, 8)
        masked = work(large_model.request_shape(512, 512, with_reference=False, max_sequence_length=512, steps=4), self._options(text_attention_in_mask=True))
        self.assertEqual(masked.extra_device_bytes, attention.text_attention_mask_bytes(512, 1024, 0, "bfloat16"))

    def test_decode_work_follows_what_the_vae_will_really_do(self) -> None:
        work = large_model.pipeline_spec("bfloat16").phases[1].work
        tall = large_model.request_shape(1152, 896, with_reference=False, max_sequence_length=512, steps=4)
        square = large_model.request_shape(768, 768, with_reference=False, max_sequence_length=512, steps=4)
        self.assertEqual(work(tall, self._options(vae_tiling=False)).pixels, 1152 * 896)
        self.assertEqual(work(tall, self._options(vae_tiling=True)).pixels, 1024 * 896, "one tile alive at a time")
        self.assertEqual(work(square, self._options(vae_tiling=True)).pixels, 768 * 768, "below the threshold tiling does not engage")
        lowered = large_model.pipeline_spec("bfloat16").fallbacks[1].apply(self._options(vae_tiling=False))
        self.assertEqual(work(square, lowered).pixels, 512 * 512)
        self.assertEqual(work(tall, lowered).pixels, 512 * 512)

    def test_the_fallbacks_only_switch_options_on(self) -> None:
        tiling, lowered = large_model.pipeline_spec("bfloat16").fallbacks
        options = self._options(vae_tiling=False)
        self.assertEqual((tiling.apply(options).vae_tiling, tiling.apply(options).vae_tile_lowered), (True, False))
        self.assertEqual((lowered.apply(options).vae_tiling, lowered.apply(options).vae_tile_lowered), (True, True))
        self.assertFalse(options.vae_tiling, "the argument is not mutated")

    def test_the_keys_name_the_loaded_bytes(self) -> None:
        pipeline = large_model.pipeline_spec("bfloat16")
        options = self._options()
        key = pipeline.key(options, large_model.WEIGHT_FORMAT, "cuda:0")
        self.assertIn(self.paths["transformer_path"], key)
        self.assertNotEqual(key, pipeline.key(options, large_model.WEIGHT_FORMAT, "cuda:1"))
        encoder = large_model.encoder_spec(True)
        encoder_options = large_model.encoder_options(svc.normalize_flux2_klein_params(self.params(text_encoder_fp8=True)))
        self.assertIn("fp8", encoder.key(encoder_options, large_model.WEIGHT_FORMAT, "cuda:0"))
        # The CPU-only spec never shares a handle with the other one (the manager keys handles globally).
        normalized = svc.normalize_flux2_klein_params(self.params())
        pinned = large_model.encoder_options(normalized, cpu_only=True)
        unpinned = large_model.encoder_options(normalized)
        self.assertNotEqual(large_model.encoder_spec(False, cpu_only=True).key(pinned, large_model.WEIGHT_FORMAT, "cuda:0"), large_model.encoder_spec(False).key(unpinned, large_model.WEIGHT_FORMAT, "cuda:0"))


class RequireLoadableTests(_TempTreeCase):
    def _normalized(self, **overrides: object) -> dict[str, object]:
        return svc.normalize_flux2_klein_params(self.params(**overrides))

    def test_the_default_tree_is_loadable(self) -> None:
        large_model.require_loadable(self._normalized(), encoder=True)

    def test_an_fp8_scaled_file_is_refused(self) -> None:
        checkpoint = self.root / "fp8.safetensors"
        _write_safetensors(checkpoint, {"img_in.weight": {"dtype": "F8_E4M3", "shape": [1], "data_offsets": [0, 1]}, "img_in.weight_scale": {"dtype": "F32", "shape": [1], "data_offsets": [1, 5]}})
        (self.root / "config.json").write_text(json.dumps({"_class_name": "Flux2Transformer2DModel"}), encoding="utf-8")
        with self.assertRaises(ValueError):
            large_model.require_loadable(self._normalized(transformer_path=str(checkpoint)), encoder=False)

    def test_a_prefixed_checkpoint_is_refused_by_name(self) -> None:
        checkpoint = self.root / "prefixed.safetensors"
        _write_header_only(checkpoint, {"model.diffusion_model.img_in.weight": ("BF16", (2, 2))})
        (self.root / "config.json").write_text(json.dumps({"_class_name": "Flux2Transformer2DModel"}), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "model.diffusion_model."):
            large_model.require_loadable(self._normalized(transformer_path=str(checkpoint)), encoder=False)

    def test_a_single_file_without_its_config_is_refused(self) -> None:
        outside = tempfile.TemporaryDirectory()
        self.addCleanup(outside.cleanup)
        folder = Path(outside.name) / "lonely"
        folder.mkdir()
        checkpoint = folder / "klein.safetensors"
        _write_header_only(checkpoint, {"img_in.weight": ("BF16", (2, 2))})
        with self.assertRaises(FileNotFoundError):
            large_model.require_loadable(self._normalized(transformer_path=str(checkpoint)), encoder=False)

    def test_a_vae_file_without_its_folder_config_is_refused(self) -> None:
        loose = self.root / "loose_vae.safetensors"
        _write_header_only(loose, {"decoder.conv_in.weight": ("BF16", (2, 2))})
        with self.assertRaisesRegex(ValueError, "VAE"):
            large_model.require_loadable(self._normalized(vae_path=str(loose)), encoder=False)

    def test_a_shallow_encoder_is_refused(self) -> None:
        (Path(self.paths["text_encoder_path"]) / "config.json").write_text(json.dumps({"num_hidden_layers": 4}), encoding="utf-8")
        with self.assertRaises(ValueError):
            large_model.require_loadable(self._normalized(), encoder=True)


@unittest.skipUnless(_has("torch", "diffusers", "transformers", "accelerate"), "needs torch, diffusers, transformers and accelerate")
class RealLoadTests(_TempTreeCase):
    """The tiny real tree loaded through the manager on the CPU, per component, as a run would."""

    real_weights = True

    def _manager(self) -> LargeModelManager:
        from modules.ai_backend.runtime.large_models.loader import TorchTransport

        probes = _RoomyProbes()
        manager = LargeModelManager(ResidencyArbiter(probes), limits_store=MemoryLimitsStore(), tables=_flat_tables(), probes=probes, transport_factory=lambda _device: TorchTransport("cpu"), device_class=lambda _device: "test")
        large_model.register_specs(manager)
        return manager

    def test_every_component_loads_with_every_state_key_filled(self) -> None:
        import torch

        manager = self._manager()
        normalized = svc.normalize_flux2_klein_params(self.params())
        shape = large_model.request_shape(128, 128, with_reference=False, max_sequence_length=64, steps=1)
        for model, options, names in (
            ("flux2_klein", large_model.pipeline_options(normalized, guidance=False), ("transformer", "vae")),
            ("flux2_klein_text_encoder", large_model.encoder_options(normalized), ("text_encoder",)),
            ("flux2_klein_text_encoder.fp8", large_model.encoder_options(svc.normalize_flux2_klein_params(self.params(text_encoder_fp8=True))), ("text_encoder",)),
        ):
            if model.endswith(".fp8") and getattr(torch, "float8_e4m3fn", None) is None:
                continue
            with self.subTest(model=model), manager.acquire(model, options, shape, device="cuda:0", format="bf16") as session:
                session.load()
                for name in names:
                    module = session.module(name)
                    stray = [key for key, value in [*module.named_parameters(), *module.named_buffers()] if value.is_meta]
                    self.assertEqual(stray, [], f"{name}: tensors left on meta")
                if model == "flux2_klein":
                    self.assertEqual(session.module("transformer").dtype, torch.bfloat16)
                if model.startswith("flux2_klein_text_encoder"):
                    encoder = session.module("text_encoder")
                    self.assertEqual(len(encoder.layers), svc.ENCODER_KEEP_LAYERS)
                    with torch.no_grad():
                        out = encoder(input_ids=torch.tensor([[1, 2, 3]]), output_hidden_states=True)
                    self.assertEqual(len(out.hidden_states), svc.ENCODER_KEEP_LAYERS + 1)


if __name__ == "__main__":
    unittest.main()
