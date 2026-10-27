"""
File: modules/ai_backend/inpaint/flux2_klein/test_encoder_gguf.py

Purpose:
Unit tests for the GGUF text encoder of FLUX.2 klein (the name bridge itself,
`runtime/qwen3_gguf.py`, is tested by `runtime/test_qwen3_gguf.py`): the encoder
spec over a GGUF file (`large_model.py`:
formats, keys, the D7 encode variant, the D8 fp8 refusal, the pre-admission
refusals, the GGUF module swap), a tiny REAL Qwen3 written as a GGUF and
loaded through the real manager on the CPU, which must compute exactly like
the same model built from the dequantized weights (also under «Энкодер на CPU»),
and the RECORDED uncensored 9B encoder header (`fixtures/flux2_9b_gguf_headers.json`,
written header-only): its 399 names, its quantized keys, its strict match with
the stock 9B config and the package fallback that picks the 9B package for it.

Notes:
The tests need gguf, torch, transformers and accelerate (the bridge's key set
is the real `Qwen3Model`'s). Each skips
cleanly without them. No GPU, no network; files go to temporary directories.
"""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from modules.ai_backend.inpaint import flux2_klein as svc
from modules.ai_backend.inpaint.flux2_klein import components, large_model
from modules.ai_backend.runtime import qwen3_gguf
from modules.ai_backend.runtime.large_models.manager import LargeModelManager, MemoryLimitsStore
from modules.ai_backend.runtime.residency.arbiter import ResidencyArbiter

from ._test_fixtures import TINY_GGUF_ENCODER_CONFIG, _RoomyProbes, _TempTreeCase, _flat_tables, _write_qwen3_gguf_encoder, load_9b_config, load_9b_header_fixture, write_header_only_gguf


def _has(*modules: str) -> bool:
    try:
        for name in modules:
            __import__(name)
    except ImportError:
        return False
    return True


_NAME_STACK = ("gguf", "torch", "transformers", "accelerate")

class _GgufEncoderCase(_TempTreeCase):
    """A tree whose `text_encoder/config.json` is the tiny GGUF encoder's, plus that encoder as `<root>/text_encoder_gguf/qwen3-q8.gguf`."""

    def setUp(self) -> None:
        super().setUp()
        (self.root / "text_encoder" / "config.json").write_text(json.dumps(TINY_GGUF_ENCODER_CONFIG), encoding="utf-8")
        self.gguf = self.root / "text_encoder_gguf" / "qwen3-q8.gguf"
        self.reference = _write_qwen3_gguf_encoder(self.gguf)
        self.formats = [{"id": "bf16", "path": self.paths["text_encoder_path"]}, {"id": "q8_0", "path": str(self.gguf)}]

    def normalized(self, **overrides: object) -> dict[str, object]:
        params = self.params(text_encoder_path=str(self.gguf), text_encoder_format="q8_0", text_encoder_formats=self.formats)
        params.update(overrides)
        return svc.normalize_flux2_klein_params(params)


@unittest.skipUnless(_has(*_NAME_STACK), "needs gguf, torch, transformers and accelerate")
class GgufEncoderSpecTests(_GgufEncoderCase):
    def test_options_formats_and_keys(self) -> None:
        options = large_model.encoder_options(self.normalized())
        self.assertEqual(large_model.weight_format_of(options), "q8_0")
        self.assertEqual(large_model.encoder_path_for(options, "bf16"), self.paths["text_encoder_path"])
        rows = large_model.encoder_spec(False).formats(options)
        self.assertEqual(list(rows), ["bf16", "q8_0"])
        self.assertEqual((rows["bf16"].compute_path, rows["bf16"].quality_rank), (large_model.COMPUTE_NATIVE, 2))
        self.assertEqual((rows["q8_0"].compute_path, rows["q8_0"].quality_rank, rows["q8_0"].dit_bytes), (large_model.COMPUTE_GGUF, 1, self.gguf.stat().st_size))
        self.assertTrue(all(row.available for row in rows.values()))
        self.assertEqual(list(large_model.encoder_spec(True).formats(options)), ["bf16"], "fp8 is a conversion of bf16 weights only (D8)")
        spec = large_model.encoder_spec(False)
        self.assertIn(str(self.gguf), spec.key(options, "q8_0", "cpu"))
        self.assertIn(self.paths["text_encoder_path"], spec.key(options, "bf16", "cpu"))
        bf16_options = large_model.encoder_options(svc.normalize_flux2_klein_params(self.params()))
        self.assertEqual(large_model._encoder_key(bf16_options, "bf16", "cpu"), f"flux2_klein_text_encoder:bf16|{self.paths['text_encoder_path']}|cpu", "the bf16 key is unchanged")
        with self.assertRaises(ValueError):
            large_model.encoder_path_for(options, "q4_k_m")

    def test_the_encode_variant_marks_a_gguf_encoder(self) -> None:
        shape = large_model.request_shape(64, 64, with_reference=False, max_sequence_length=64, steps=1)
        self.assertEqual(large_model._encode_work(shape, large_model.encoder_options(self.normalized())).variant, large_model.ENCODE_VARIANT_GGUF)
        bf16 = large_model.encoder_options(svc.normalize_flux2_klein_params(self.params()))
        self.assertEqual(large_model._encode_work(shape, bf16).variant, large_model.ENCODE_VARIANT)

    def test_fp8_with_a_gguf_encoder_is_refused_naming_both_fields(self) -> None:
        normalized = self.normalized(text_encoder_fp8=True)
        for call in (lambda: large_model.encoder_options(normalized), lambda: large_model.require_loadable_encoder(normalized)):
            with self.subTest(call=call), self.assertRaises(ValueError) as caught:
                call()
            self.assertIn("text_encoder_fp8", str(caught.exception))
            self.assertIn("text_encoder_path", str(caught.exception))

    def test_the_gguf_encoder_is_loadable_and_its_refusals_name_the_file(self) -> None:
        large_model.require_loadable_encoder(self.normalized())
        foreign = self.root / "text_encoder_gguf" / "llama.gguf"
        _write_qwen3_gguf_encoder(foreign, metadata={"general.architecture": "llama"})
        with self.assertRaisesRegex(ValueError, "llama"):
            large_model.require_loadable_encoder(self.normalized(text_encoder_path=str(foreign), text_encoder_formats=None, text_encoder_format=None))
        shallow = self.root / "text_encoder_gguf" / "shallow.gguf"
        _write_qwen3_gguf_encoder(shallow, metadata={"qwen3.block_count": 12})
        with self.assertRaisesRegex(ValueError, "12"):
            large_model.require_loadable_encoder(self.normalized(text_encoder_path=str(shallow), text_encoder_formats=None, text_encoder_format=None))

    def test_the_meta_model_swaps_the_linears_and_the_embedding(self) -> None:
        from modules.ai_backend.runtime.gguf_layers import GgufEmbedding, GgufLinear

        options = large_model.encoder_options(self.normalized())
        model = large_model.encoder_spec(False).component("text_encoder").build_meta(options, "q8_0")  # type: ignore[misc]
        self.assertEqual(len(model.layers), svc.ENCODER_KEEP_LAYERS)
        self.assertIsInstance(model.embed_tokens, GgufEmbedding)
        linears = [name for name, module in model.named_modules() if isinstance(module, GgufLinear)]
        self.assertEqual(len(linears), svc.ENCODER_KEEP_LAYERS * 7)
        bf16 = large_model.encoder_spec(False).component("text_encoder").build_meta(options, "bf16")  # type: ignore[misc]
        self.assertFalse(any(isinstance(module, (GgufLinear, GgufEmbedding)) for module in bf16.modules()))

    def test_the_inventory_reads_no_truncated_block(self) -> None:
        from modules.ai_backend.runtime.large_models.inventory import build_inventory

        inventory = build_inventory(large_model.encoder_spec(False).component("text_encoder"), [self.gguf])
        self.assertEqual(inventory.block_count, svc.ENCODER_KEEP_LAYERS)
        names = {entry.key for spans in inventory.blocks.values() for entry in spans} | {entry.key for entry in inventory.non_block}
        self.assertFalse(any(name.startswith(("blk.28.", "blk.29.")) for name in names))


@unittest.skipUnless(_has(*_NAME_STACK), "needs gguf, torch, transformers and accelerate")
class GgufEncoderLoadTests(_GgufEncoderCase):
    """The tiny GGUF encoder through the real manager on the CPU: hidden states equal the dequantized model's."""

    def _manager(self) -> LargeModelManager:
        from modules.ai_backend.runtime.large_models.loader import TorchTransport

        probes = _RoomyProbes()
        manager = LargeModelManager(ResidencyArbiter(probes), limits_store=MemoryLimitsStore(), tables=_flat_tables(), probes=probes, transport_factory=lambda _device: TorchTransport("cpu"), device_class=lambda _device: "test")
        large_model.register_specs(manager)
        return manager

    def _reference_model(self) -> object:
        import torch
        from transformers import Qwen3Config, Qwen3Model

        values = {key: value for key, value in TINY_GGUF_ENCODER_CONFIG.items() if key not in ("architectures", "model_type")}
        values["num_hidden_layers"] = svc.ENCODER_KEEP_LAYERS
        config = Qwen3Config(**values)
        config._attn_implementation = "sdpa"
        model = Qwen3Model(config)
        # The rotary frequencies stay float32, as in the managed model (a
        # non-persistent buffer the loader never casts); only the weights are bf16.
        inv_freq = model.rotary_emb.inv_freq.clone()
        model = model.to(torch.bfloat16)
        model.rotary_emb.inv_freq = inv_freq
        model.load_state_dict(self.reference, strict=True)
        return model.eval()

    def test_a_managed_load_computes_like_the_dequantized_model(self) -> None:
        import torch

        from modules.ai_backend.runtime.gguf_layers import GgufEmbedding, GgufLinear
        from modules.ai_backend.runtime.large_models.tiers import RamStep, Steps, VramStep

        ids = torch.tensor([[1, 5, 9, 127, 5, 0]])
        with torch.no_grad():
            expected = self._reference_model()(input_ids=ids, output_hidden_states=True).hidden_states
        shape = large_model.request_shape(64, 64, with_reference=False, max_sequence_length=64, steps=1)
        manager = self._manager()
        for on_cpu in (False, True):
            options = large_model.encoder_options(self.normalized(encoder_on_cpu=on_cpu))
            for steps in (Steps(VramStep.MAX, RamStep.KEEP), Steps(VramStep.MIN, RamStep.MIN)):
                with self.subTest(on_cpu=on_cpu, steps=steps.label):
                    with manager.acquire(large_model.encoder_model_name(False), options, shape, device="cuda:0", format=large_model.weight_format_of(options), steps=steps) as session:
                        self.assertEqual(session.weight_format, "q8_0")
                        session.load(large_model.TEXT_ENCODER)

                        def encode(ctx: object) -> object:
                            encoder = ctx.module(large_model.TEXT_ENCODER)  # type: ignore[attr-defined]
                            self.assertIsInstance(encoder.embed_tokens, GgufEmbedding)
                            # diffusers' `_get_qwen_prompt_embeds` casts the embedding to `text_encoder.dtype`.
                            self.assertEqual(encoder.dtype, torch.bfloat16)
                            self.assertEqual({module.weight.dtype for module in encoder.modules() if isinstance(module, GgufLinear)}, {torch.uint8}, "the encoder stays quantized in memory")
                            with torch.no_grad():
                                return encoder(input_ids=ids, output_hidden_states=True).hidden_states

                        produced = session.run_phase(large_model.PHASE_ENCODE, encode)
                    self.assertEqual(len(produced), len(expected))
                    for index in (*components.TEXT_ENCODER_OUT_LAYER_INDICES, len(expected) - 1):
                        self.assertTrue(torch.equal(produced[index], expected[index]), f"layer {index}: max abs diff {(produced[index].float() - expected[index].float()).abs().max().item()}")
                    manager.unload()


@unittest.skipUnless(_has(*_NAME_STACK), "needs gguf, torch, transformers and accelerate")
class GgufEncoderConfigDiscoveryTests(_GgufEncoderCase):
    """Where a GGUF encoder's `config.json` comes from: beside it, else an installed klein package, never an inconsistent one."""

    def _outside(self) -> Path:
        outside = tempfile.TemporaryDirectory()
        self.addCleanup(outside.cleanup)
        path = Path(outside.name) / "sd_models" / "Qwen3-tiny-Q8_0.gguf"
        _write_qwen3_gguf_encoder(path)
        return path

    def _install_package_config(self, config: dict[str, object]) -> Path:
        folder = self.installed_package / "text_encoder"
        folder.mkdir(parents=True)
        (folder / "config.json").write_text(json.dumps(config), encoding="utf-8")
        return folder

    def test_a_file_in_the_package_finds_the_package_config(self) -> None:
        self.assertEqual(components.find_text_encoder_config_dir(self.gguf), self.root / "text_encoder")

    def test_a_file_outside_falls_back_to_the_installed_package(self) -> None:
        path = self._outside()
        folder = self._install_package_config(TINY_GGUF_ENCODER_CONFIG)
        with self.assertLogs(components.log, "DEBUG") as logs:
            self.assertEqual(components.find_text_encoder_config_dir(path), folder)
        self.assertTrue(any("из установленного пакета" in line for line in logs.output), "the borrowed config is logged")

    def test_an_inconsistent_package_config_is_refused_by_name(self) -> None:
        path = self._outside()
        self._install_package_config({**TINY_GGUF_ENCODER_CONFIG, "num_hidden_layers": 36})
        with self.assertRaises(FileNotFoundError) as caught:
            components.find_text_encoder_config_dir(path)
        message = str(caught.exception)
        self.assertIn(path.name, message)
        self.assertIn("слоёв: в файле 30, в config.json 36", message)

    def test_no_config_anywhere_is_refused(self) -> None:
        with self.assertRaisesRegex(FileNotFoundError, "Просмотрены каталоги"):
            components.find_text_encoder_config_dir(self._outside())

    def test_the_package_fallback_must_declare_every_required_field(self) -> None:
        path = self._outside()
        self._install_package_config({key: value for key, value in TINY_GGUF_ENCODER_CONFIG.items() if key != "num_key_value_heads"})
        with self.assertRaisesRegex(FileNotFoundError, "голов ключей: не объявлено"):
            components.find_text_encoder_config_dir(path)

    def test_the_compatibility_guard_reads_the_found_config(self) -> None:
        # 3 x 64 = 192 does not match a transformer declaring 24: provable through the GGUF's config.
        (self.root / "transformer" / "config.json").write_text(json.dumps({"joint_attention_dim": 24}), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "hidden_size = 64"):
            components.require_encoder_transformer_compatible({"text_encoder_path": str(self.gguf), "transformer_path": self.paths["transformer_path"]})
        (self.root / "transformer" / "config.json").write_text(json.dumps({"joint_attention_dim": 192}), encoding="utf-8")
        components.require_encoder_transformer_compatible({"text_encoder_path": str(self.gguf), "transformer_path": self.paths["transformer_path"]})
        # No config for an outside GGUF: silent here, refused by `require_loadable_encoder`.
        components.require_encoder_transformer_compatible({"text_encoder_path": str(self._outside()), "transformer_path": self.paths["transformer_path"]})


#: The shipped Qwen3-4B `text_encoder/config.json` of the klein 4B package,
#: reduced to the fields the GGUF config match reads.
_KLEIN_4B_ENCODER_CONFIG = {"architectures": ["Qwen3ForCausalLM"], "head_dim": 128, "hidden_size": 2560, "intermediate_size": 9728, "num_attention_heads": 32, "num_hidden_layers": 36, "num_key_value_heads": 8, "vocab_size": 151936}


@unittest.skipUnless(_has(*_NAME_STACK), "needs gguf, torch, transformers and accelerate")
class UncensoredNineBEncoderHeaderTests(_TempTreeCase):
    """The recorded uncensored 9B Q8_0 encoder header: names, quantized keys, config match and the 9B package fallback."""

    def setUp(self) -> None:
        super().setUp()
        outside = tempfile.TemporaryDirectory()
        self.addCleanup(outside.cleanup)
        header = load_9b_header_fixture("unc_q8.head")
        self.path = write_header_only_gguf(Path(outside.name) / "sd_models" / "flux2-klein-9b-uncensored-q8_0.gguf", header["metadata"], header["tensors"])  # type: ignore[arg-type]
        self.layout = components.gguf_layout(self.path)

    def test_every_name_maps_and_the_tied_head_is_dropped(self) -> None:
        keep = svc.ENCODER_KEEP_LAYERS
        names = [span.name for span in self.layout.spans]
        self.assertEqual(len(names), 399)
        self.assertIn("output.weight", names)
        self.assertIsNone(qwen3_gguf.encoder_state_key_of_gguf("output.weight", keep))
        qwen3_gguf.require_known_encoder_names(names, keep, source=self.path, owner="FLUX.2 klein", logger=svc.log)

    def test_the_quantized_keys_are_every_kept_linear_and_the_embedding(self) -> None:
        keep = svc.ENCODER_KEEP_LAYERS
        quantized = qwen3_gguf.gguf_encoder_quant_types(self.layout.spans, keep)
        linears = ("self_attn.q_proj", "self_attn.k_proj", "self_attn.v_proj", "self_attn.o_proj", "mlp.gate_proj", "mlp.up_proj", "mlp.down_proj")
        expected = {"embed_tokens.weight", *(f"layers.{index}.{linear}.weight" for index in range(keep) for linear in linears)}
        self.assertEqual(set(quantized), expected)
        self.assertEqual(set(quantized.values()), {8}, "Q8_0 throughout")

    def test_the_stock_9b_config_matches_strictly_and_the_4b_one_does_not(self) -> None:
        self.assertIsNone(components._gguf_encoder_mismatch(load_9b_config("text_encoder"), self.layout, True))
        self.assertEqual(components._gguf_encoder_mismatch(_KLEIN_4B_ENCODER_CONFIG, self.layout, True), "ширина: в файле 4096, в config.json 2560")

    def test_the_fallback_takes_the_9b_package_and_never_the_4b_one(self) -> None:
        for package, config in ((self.installed_package, _KLEIN_4B_ENCODER_CONFIG), (self.installed_package_9b, load_9b_config("text_encoder"))):
            folder = package / "text_encoder"
            folder.mkdir(parents=True)
            (folder / "config.json").write_text(json.dumps(config), encoding="utf-8")
        self.assertEqual(components.find_text_encoder_config_dir(self.path), self.installed_package_9b / "text_encoder")
        large_model._require_loadable_gguf_encoder(self.path, fp8=False)


if __name__ == "__main__":
    unittest.main()
