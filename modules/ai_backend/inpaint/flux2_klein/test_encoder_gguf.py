"""
File: modules/ai_backend/inpaint/flux2_klein/test_encoder_gguf.py

Purpose:
Unit tests for the GGUF text encoder of FLUX.2 klein: the llama.cpp <-> HF
Qwen3 name bridge (`encoder_gguf.py`: full 398-name coverage, truncation, the
dropped head, refusals), the encoder spec over a GGUF file (`large_model.py`:
formats, keys, the D7 encode variant, the D8 fp8 refusal, the pre-admission
refusals, the GGUF module swap) and a tiny REAL Qwen3 written as a GGUF and
loaded through the real manager on the CPU, which must compute exactly like
the same model built from the dequantized weights (also under «Энкодер на CPU»).

Notes:
The name-bridge tests need gguf, transformers and accelerate (the key set is
the real `Qwen3Model`'s); the spec and load tests also torch. Each skips
cleanly without them. No GPU, no network; files go to temporary directories.
"""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from modules.ai_backend.inpaint import flux2_klein as svc
from modules.ai_backend.inpaint.flux2_klein import components, encoder_gguf, large_model
from modules.ai_backend.runtime.large_models.manager import LargeModelManager, MemoryLimitsStore
from modules.ai_backend.runtime.residency.arbiter import ResidencyArbiter

from ._test_fixtures import TINY_GGUF_ENCODER_CONFIG, _RoomyProbes, _TempTreeCase, _flat_tables, _write_qwen3_gguf_encoder


def _has(*modules: str) -> bool:
    try:
        for name in modules:
            __import__(name)
    except ImportError:
        return False
    return True


_NAME_STACK = ("gguf", "torch", "transformers", "accelerate")

#: The eleven per-layer tensors of a llama.cpp Qwen3 GGUF.
_LAYER_TENSORS = ("attn_k", "attn_k_norm", "attn_norm", "attn_output", "attn_q", "attn_q_norm", "attn_v", "ffn_down", "ffn_gate", "ffn_norm", "ffn_up")

#: Recorded by hand from llama.cpp's `convert_hf_to_gguf.py` naming of Qwen3
#: (independent of the `gguf` map the code inverts).
_RECORDED_PAIRS = {
    "token_embd.weight": "embed_tokens.weight",
    "output_norm.weight": "norm.weight",
    "blk.3.attn_q.weight": "layers.3.self_attn.q_proj.weight",
    "blk.3.attn_k.weight": "layers.3.self_attn.k_proj.weight",
    "blk.3.attn_v.weight": "layers.3.self_attn.v_proj.weight",
    "blk.3.attn_output.weight": "layers.3.self_attn.o_proj.weight",
    "blk.3.attn_q_norm.weight": "layers.3.self_attn.q_norm.weight",
    "blk.3.attn_k_norm.weight": "layers.3.self_attn.k_norm.weight",
    "blk.3.attn_norm.weight": "layers.3.input_layernorm.weight",
    "blk.3.ffn_norm.weight": "layers.3.post_attention_layernorm.weight",
    "blk.27.ffn_gate.weight": "layers.27.mlp.gate_proj.weight",
    "blk.27.ffn_up.weight": "layers.27.mlp.up_proj.weight",
    "blk.27.ffn_down.weight": "layers.27.mlp.down_proj.weight",
}


def _qwen3_4b_names() -> list[str]:
    """The 398 tensor names of a real Qwen3-4B GGUF (36 layers x 11, the embedding, the final norm; no `output.weight`)."""
    return ["token_embd.weight", "output_norm.weight", *(f"blk.{index}.{tensor}.weight" for index in range(36) for tensor in _LAYER_TENSORS)]


@unittest.skipUnless(_has(*_NAME_STACK), "needs gguf, torch, transformers and accelerate")
class KeyMapTests(unittest.TestCase):
    def test_every_name_of_a_real_file_maps_or_is_skipped(self) -> None:
        names = _qwen3_4b_names()
        self.assertEqual(len(names), 398)
        keep = svc.ENCODER_KEEP_LAYERS
        mapped = {name: encoder_gguf.encoder_state_key_of_gguf(name, keep) for name in names}
        skipped = [name for name, key in mapped.items() if key is None]
        self.assertEqual(len(skipped), (36 - keep) * len(_LAYER_TENSORS), "exactly the blocks past the truncation")
        self.assertTrue(all(int(name.split(".")[1]) >= keep for name in skipped))
        keys = [key for key in mapped.values() if key is not None]
        self.assertEqual(len(keys), len(set(keys)), "no two tensors share a state key")
        self.assertEqual(set(keys), encoder_gguf._expected_state_keys(keep))
        encoder_gguf.require_known_encoder_names(names, keep, source=Path("qwen3.gguf"))

    def test_recorded_pairs_and_no_renaming_of_rows(self) -> None:
        for name, key in _RECORDED_PAIRS.items():
            with self.subTest(name=name):
                self.assertEqual(encoder_gguf.encoder_state_key_of_gguf(name, 28), key)

    def test_the_head_and_the_truncation_are_dropped(self) -> None:
        self.assertIsNone(encoder_gguf.encoder_state_key_of_gguf("output.weight", 28))
        self.assertIsNone(encoder_gguf.encoder_state_key_of_gguf("blk.28.attn_q.weight", 28))
        self.assertEqual(encoder_gguf.encoder_state_key_of_gguf("blk.28.attn_q.weight", 30), "layers.28.self_attn.q_proj.weight")

    def test_unknown_names_are_refused(self) -> None:
        for name in ("rope_freqs.weight", "blk.0.attn_q.bias", "blk.0.attn_qkv.weight", "foo.weight"):
            with self.subTest(name=name), self.assertRaises(ValueError):
                encoder_gguf.encoder_state_key_of_gguf(name, 28)
        with self.assertRaisesRegex(ValueError, "rope_freqs.weight"):
            encoder_gguf.require_known_encoder_names([*_qwen3_4b_names(), "rope_freqs.weight"], 28, source=Path("x.gguf"))

    def test_a_file_missing_kept_weights_is_refused(self) -> None:
        names = [name for name in _qwen3_4b_names() if name != "blk.5.ffn_up.weight"]
        with self.assertRaisesRegex(ValueError, "layers.5.mlp.up_proj.weight"):
            encoder_gguf.require_known_encoder_names(names, 28, source=Path("x.gguf"))
        # A missing tensor past the truncation is not needed at all.
        names = [name for name in _qwen3_4b_names() if name != "blk.30.ffn_up.weight"]
        encoder_gguf.require_known_encoder_names(names, 28, source=Path("x.gguf"))

    def test_the_name_spaces_are_disjoint(self) -> None:
        for name in ("blk.0.attn_q.weight", "token_embd.weight", "output_norm.weight", "output.weight"):
            self.assertTrue(encoder_gguf.is_gguf_encoder_name(name), name)
        for name in ("model.layers.0.self_attn.q_proj.weight", "lm_head.weight", "layers.0.mlp.up_proj.weight", "embed_tokens.weight", "norm.weight"):
            self.assertFalse(encoder_gguf.is_gguf_encoder_name(name), name)


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
    """Where a GGUF encoder's `config.json` comes from: beside it, else the installed 4B package, never an inconsistent one."""

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


if __name__ == "__main__":
    unittest.main()
