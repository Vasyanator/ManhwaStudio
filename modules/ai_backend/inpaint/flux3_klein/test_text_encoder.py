"""
File: modules/ai_backend/inpaint/flux3_klein/test_text_encoder.py

Purpose:
Unit tests for `flux3_klein/text_encoder.py` on a tiny random Qwen3-VL (text
hidden 16, 36 decoder layers, a 2-block vision tower with deepstack) written as
two synthetic shards plus an index: TEXT and VL_REFS loading (key coverage, no
meta tensors, 33 layers, the vision tower never built in TEXT mode), parity of
`run_text_encoder` with the full-depth model's `hidden_states[k]` in both modes
(and that 32 layers would NOT be enough), the padding / position-id rules, the
VL budget math and refusals and the loader refusals.

Notes:
CPU only, float32 weights (so parity is exact). Tokenization with the real
processor lives in `test_real_processor.py`. Every file is written into a
temporary directory removed by the test.
"""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from typing import Any

import torch
from safetensors.torch import save_file
from transformers import Qwen3VLConfig
from transformers.models.qwen3_vl.modeling_qwen3_vl import Qwen3VLModel, Qwen3VLTextModel

from . import text_encoder as te
from .config import TEXT_MAX_TOKENS, TEXT_OUTPUT_LAYERS
from .params import EncoderMode

#: Tiny token ids; the special ones sit at the top of the 64-entry vocabulary.
_VOCAB = 64
_VISION_START = 58
_VISION_END = 59
_IMAGE_TOKEN = 60
_VIDEO_TOKEN = 61
_PAD = 62
_HIDDEN = 16
_FULL_LAYERS = 36


def _tiny_config(layers: int = _FULL_LAYERS) -> Qwen3VLConfig:
    """A random-sized Qwen3-VL with the real model's structure (mRoPE, deepstack)."""
    return Qwen3VLConfig(
        text_config={
            "hidden_size": _HIDDEN,
            "intermediate_size": 32,
            "num_hidden_layers": layers,
            "num_attention_heads": 2,
            "num_key_value_heads": 1,
            "head_dim": 8,
            "vocab_size": _VOCAB,
            "max_position_embeddings": 4096,
            "rope_theta": 5000000,
            "rope_scaling": {"rope_type": "default", "mrope_section": [2, 1, 1], "mrope_interleaved": True},
            "tie_word_embeddings": True,
        },
        vision_config={
            "depth": 2,
            "hidden_size": 16,
            "intermediate_size": 32,
            "num_heads": 2,
            "in_channels": 3,
            "patch_size": 16,
            "spatial_merge_size": 2,
            "temporal_patch_size": 2,
            "out_hidden_size": _HIDDEN,
            "num_position_embeddings": 16,
            "deepstack_visual_indexes": [0, 1],
        },
        image_token_id=_IMAGE_TOKEN,
        video_token_id=_VIDEO_TOKEN,
        vision_start_token_id=_VISION_START,
        vision_end_token_id=_VISION_END,
        tie_word_embeddings=True,
        attn_implementation="sdpa",
    )


def _full_model(config: Qwen3VLConfig) -> Qwen3VLModel:
    """The full-depth reference model, float32, deterministic weights."""
    torch.manual_seed(1234)
    model = Qwen3VLModel(config).eval()
    # The default init leaves norms at exactly 1 and some biases at 0; random
    # values make every tensor's placement observable in the parity checks.
    with torch.no_grad():
        for parameter in model.parameters():
            parameter.normal_(0.0, 0.2)
    return model


def _write_checkpoint(directory: Path, config: Qwen3VLConfig, model: Qwen3VLModel) -> dict[str, torch.Tensor]:
    """Write `config.json`, two shards and the index with the real export's key names."""
    config.save_pretrained(directory)
    state = {f"model.{key}": value.detach().clone().contiguous() for key, value in model.state_dict().items()}
    first = {key: value for key, value in state.items() if key.startswith("model.language_model.")}
    second = {key: value for key, value in state.items() if key not in first}
    names = ("model-00001-of-00002.safetensors", "model-00002-of-00002.safetensors")
    save_file(first, str(directory / names[0]))
    save_file(second, str(directory / names[1]))
    weight_map = {key: names[0] for key in first} | {key: names[1] for key in second}
    (directory / "model.safetensors.index.json").write_text(
        json.dumps({"metadata": {}, "weight_map": weight_map}), encoding="utf-8"
    )
    return state


def _handle(model: Any, mode: EncoderMode, keep_layers: int = te.ENCODER_KEEP_LAYERS) -> te.TextEncoderHandle:
    """A model-only handle on the CPU."""
    return te.TextEncoderHandle(
        model=model, processor=None, mode=mode, device=torch.device("cpu"), keep_layers=keep_layers
    )


def _text_batch(length: int = 80, real: int = 37) -> te.TokenBatch:
    """A hand-made right-padded text batch of ordinary token ids."""
    generator = torch.Generator().manual_seed(7)
    ids = torch.randint(0, _VISION_START, (1, real), generator=generator)
    padded, mask = te.right_pad(ids, length=length, pad_id=_PAD)
    return te.TokenBatch(input_ids=padded, attention_mask=mask, real_length=real)


def _vl_batch() -> te.TokenBatch:
    """Two 64x64 images (grid 1x4x4 -> 4 LLM tokens each) before a short caption, padded to 80."""
    generator = torch.Generator().manual_seed(11)
    prefix = torch.randint(0, _VISION_START, (3,), generator=generator).tolist()
    caption = torch.randint(0, _VISION_START, (9,), generator=generator).tolist()
    image = [_VISION_START] + [_IMAGE_TOKEN] * 4 + [_VISION_END]
    ids = torch.tensor([prefix + image + image + caption])
    padded, mask = te.right_pad(ids, length=te.padded_length(int(ids.shape[1])), pad_id=_PAD)
    grid = torch.tensor([[1, 4, 4], [1, 4, 4]])
    # 16 patches per image, each 3 channels x temporal 2 x 16 x 16.
    pixels = torch.randn(32, 3 * 2 * 16 * 16, generator=generator)
    return te.TokenBatch(
        input_ids=padded,
        attention_mask=mask,
        real_length=int(ids.shape[1]),
        pixel_values=pixels,
        image_grid_thw=grid,
    )


def _reference_stack(hidden_states: tuple[torch.Tensor, ...], length: int) -> torch.Tensor:
    """The reference's `(L, 8 * hidden)` bf16 stacking of `TEXT_OUTPUT_LAYERS`."""
    stacked = torch.stack([hidden_states[k] for k in TEXT_OUTPUT_LAYERS], dim=1)
    return stacked.permute(0, 2, 1, 3).reshape(length, -1).to(torch.bfloat16)


class _Checkpoint(unittest.TestCase):
    """One tiny checkpoint and its full-depth model, shared by a test class."""

    root: Path
    config: Qwen3VLConfig
    full: Qwen3VLModel
    state: dict[str, torch.Tensor]

    @classmethod
    def setUpClass(cls) -> None:
        cls._tmp = tempfile.TemporaryDirectory()
        cls.addClassCleanup(cls._tmp.cleanup)
        cls.root = Path(cls._tmp.name)
        cls.config = _tiny_config()
        cls.full = _full_model(cls.config)
        cls.state = _write_checkpoint(cls.root, cls.config, cls.full)

    def _load(self, mode: EncoderMode, **kwargs: Any) -> tuple[Any, Any]:
        return te.load_text_encoder_model(self.root, mode=mode, device="cpu", dtype=torch.float32, **kwargs)

    def _keys(self, predicate: Any) -> int:
        return sum(1 for key in self.state if predicate(key))


def _is_dropped_layer(key: str) -> bool:
    """A language decoder layer at or above the kept count."""
    prefix = "model.language_model.layers."
    if not key.startswith(prefix):
        return False
    return int(key[len(prefix) :].split(".", 1)[0]) >= te.ENCODER_KEEP_LAYERS


class LoadTests(_Checkpoint):
    def test_keep_layers_is_33(self) -> None:
        self.assertEqual(te.ENCODER_KEEP_LAYERS, 33)

    def test_text_mode_builds_only_the_truncated_language_model(self) -> None:
        model, report = self._load(EncoderMode.TEXT)
        self.assertIsInstance(model, Qwen3VLTextModel)
        self.assertEqual(len(model.layers), 33)
        self.assertEqual(model.config.num_hidden_layers, 33)
        self.assertEqual(model.config._attn_implementation, "sdpa")
        self.assertFalse(hasattr(model, "visual"))
        self.assertEqual(report.loaded, len(model.state_dict()))
        dropped = self._keys(lambda key: key.startswith("model.visual.") or _is_dropped_layer(key))
        self.assertEqual(report.skipped, dropped)
        self.assertGreater(self._keys(_is_dropped_layer), 0)
        self.assertFalse(model.training)

    def test_vl_mode_builds_the_vision_tower_and_the_truncated_language_model(self) -> None:
        model, report = self._load(EncoderMode.VL_REFS)
        self.assertIsInstance(model, Qwen3VLModel)
        self.assertEqual(len(model.language_model.layers), 33)
        self.assertEqual(len(model.visual.blocks), 2)
        self.assertEqual(report.loaded, len(model.state_dict()))
        self.assertEqual(report.skipped, self._keys(_is_dropped_layer))

    def test_no_meta_tensor_is_left_and_rotary_buffers_are_real(self) -> None:
        for mode in EncoderMode:
            model, _ = self._load(mode)
            leftovers = [name for name, value in [*model.named_parameters(), *model.named_buffers()] if value.is_meta]
            self.assertEqual(leftovers, [], mode)
            buffers = dict(model.named_buffers())
            inv_freq = [name for name in buffers if name.endswith("inv_freq")]
            self.assertTrue(inv_freq, mode)
            for name in inv_freq:
                self.assertEqual(buffers[name].dtype, torch.float32, name)

    def test_weights_take_the_requested_dtype(self) -> None:
        model, _ = te.load_text_encoder_model(self.root, mode=EncoderMode.TEXT, device="cpu", dtype=torch.bfloat16)
        self.assertEqual({parameter.dtype for parameter in model.parameters()}, {torch.bfloat16})
        loaded = model.state_dict()["layers.5.mlp.up_proj.weight"]
        expected = self.state["model.language_model.layers.5.mlp.up_proj.weight"].to(torch.bfloat16)
        self.assertTrue(torch.equal(loaded, expected))


class ParityTests(_Checkpoint):
    def test_text_mode_matches_the_full_model_hidden_states(self) -> None:
        model, _ = self._load(EncoderMode.TEXT)
        batch = _text_batch()
        encoded = te.run_text_encoder(_handle(model, EncoderMode.TEXT), batch)
        with torch.inference_mode():
            full = self.full(
                input_ids=batch.input_ids,
                attention_mask=batch.attention_mask,
                position_ids=te.reference_position_ids(batch.attention_mask),
                output_hidden_states=True,
                use_cache=False,
            )
        self.assertEqual(len(full.hidden_states), _FULL_LAYERS + 1)
        self.assertEqual(encoded.tokens.shape, (80, len(TEXT_OUTPUT_LAYERS) * _HIDDEN))
        self.assertEqual(encoded.tokens.dtype, torch.bfloat16)
        self.assertTrue(torch.equal(encoded.tokens, _reference_stack(full.hidden_states, 80)))
        self.assertEqual(encoded.real_length, 37)
        self.assertEqual(encoded.ids.shape, (80, 4))
        self.assertTrue(torch.equal(encoded.ids[:, :3], torch.zeros(80, 3, dtype=torch.long)))
        self.assertTrue(torch.equal(encoded.ids[:, 3], torch.arange(80)))

    def test_32_layers_would_feed_a_normed_state(self) -> None:
        # The +1 rule end-to-end: with 32 layers, hidden_states[32] is the
        # normed output, so the last block of the stack differs.
        model, _ = self._load(EncoderMode.TEXT, keep_layers=32)
        batch = _text_batch()
        short = te.run_text_encoder(_handle(model, EncoderMode.TEXT, keep_layers=32), batch)
        full, _ = self._load(EncoderMode.TEXT)
        right = te.run_text_encoder(_handle(full, EncoderMode.TEXT), batch)
        last = slice((len(TEXT_OUTPUT_LAYERS) - 1) * _HIDDEN, None)
        head = slice(0, (len(TEXT_OUTPUT_LAYERS) - 1) * _HIDDEN)
        self.assertTrue(torch.equal(short.tokens[:, head], right.tokens[:, head]))
        self.assertFalse(torch.equal(short.tokens[:, last], right.tokens[:, last]))

    def test_vl_mode_matches_the_full_model_with_images(self) -> None:
        model, _ = self._load(EncoderMode.VL_REFS)
        batch = _vl_batch()
        assert batch.pixel_values is not None and batch.image_grid_thw is not None
        encoded = te.run_text_encoder(_handle(model, EncoderMode.VL_REFS), batch)
        length = int(batch.input_ids.shape[1])
        self.assertEqual(length % 80, 0)
        with torch.inference_mode():
            full = self.full(
                input_ids=batch.input_ids,
                attention_mask=batch.attention_mask,
                pixel_values=batch.pixel_values,
                image_grid_thw=batch.image_grid_thw,
                output_hidden_states=True,
                use_cache=False,
            )
        self.assertEqual(encoded.tokens.shape, (length, len(TEXT_OUTPUT_LAYERS) * _HIDDEN))
        self.assertTrue(torch.equal(encoded.tokens, _reference_stack(full.hidden_states, length)))
        self.assertTrue(torch.equal(encoded.ids[:, 3], torch.arange(length)))

    def test_images_change_the_encoding(self) -> None:
        model, _ = self._load(EncoderMode.VL_REFS)
        handle = _handle(model, EncoderMode.VL_REFS)
        batch = _vl_batch()
        assert batch.pixel_values is not None
        other = te.TokenBatch(
            input_ids=batch.input_ids,
            attention_mask=batch.attention_mask,
            real_length=batch.real_length,
            pixel_values=batch.pixel_values + 1.0,
            image_grid_thw=batch.image_grid_thw,
        )
        first = te.run_text_encoder(handle, batch)
        second = te.run_text_encoder(handle, other)
        caption_row = batch.real_length - 1
        self.assertFalse(torch.equal(first.tokens[caption_row], second.tokens[caption_row]))

    def test_text_batch_on_a_vl_handle_equals_the_text_mode(self) -> None:
        text_model, _ = self._load(EncoderMode.TEXT)
        vl_model, _ = self._load(EncoderMode.VL_REFS)
        batch = _text_batch()
        text = te.run_text_encoder(_handle(text_model, EncoderMode.TEXT), batch)
        vl = te.run_text_encoder(_handle(vl_model, EncoderMode.VL_REFS), batch)
        self.assertTrue(torch.equal(text.tokens, vl.tokens))

    def test_images_on_a_text_handle_are_refused(self) -> None:
        model, _ = self._load(EncoderMode.TEXT)
        with self.assertRaisesRegex(ValueError, "без зрительной части"):
            te.run_text_encoder(_handle(model, EncoderMode.TEXT), _vl_batch())

    def test_a_length_off_the_pad_grid_is_refused(self) -> None:
        model, _ = self._load(EncoderMode.TEXT)
        ids, mask = te.right_pad(torch.zeros(1, 10, dtype=torch.long), length=70, pad_id=_PAD)
        batch = te.TokenBatch(input_ids=ids, attention_mask=mask, real_length=10)
        with self.assertRaisesRegex(ValueError, "кратна 80"):
            te.run_text_encoder(_handle(model, EncoderMode.TEXT), batch)


class LoaderRefusalTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)

    def test_missing_index(self) -> None:
        _tiny_config().save_pretrained(self.root)
        with self.assertRaisesRegex(FileNotFoundError, "model.safetensors.index.json"):
            te.load_text_encoder_model(self.root, mode=EncoderMode.TEXT, device="cpu")

    def test_missing_shard(self) -> None:
        _tiny_config().save_pretrained(self.root)
        (self.root / "model.safetensors.index.json").write_text(
            json.dumps({"weight_map": {"model.language_model.norm.weight": "absent.safetensors"}}), encoding="utf-8"
        )
        with self.assertRaisesRegex(FileNotFoundError, "absent.safetensors"):
            te.load_text_encoder_model(self.root, mode=EncoderMode.TEXT, device="cpu")

    def test_too_few_layers(self) -> None:
        config = _tiny_config(layers=32)
        model = _full_model(config)
        _write_checkpoint(self.root, config, model)
        with self.assertRaisesRegex(ValueError, "32 слоёв"):
            te.load_text_encoder_model(self.root, mode=EncoderMode.TEXT, device="cpu")

    def test_a_foreign_key_is_refused(self) -> None:
        config = _tiny_config()
        _write_checkpoint(self.root, config, _full_model(config))
        save_file({"lm_head.weight": torch.zeros(_VOCAB, _HIDDEN)}, str(self.root / "extra.safetensors"))
        index_path = self.root / "model.safetensors.index.json"
        index = json.loads(index_path.read_text(encoding="utf-8"))
        index["weight_map"]["lm_head.weight"] = "extra.safetensors"
        index_path.write_text(json.dumps(index), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "lm_head.weight"):
            te.load_text_encoder_model(self.root, mode=EncoderMode.TEXT, device="cpu")


class KeyMapTests(unittest.TestCase):
    def test_text_mode(self) -> None:
        key_map = te.text_mode_key_map(33)
        self.assertIsNone(key_map("model.visual.blocks.0.attn.qkv.weight"))
        self.assertIsNone(key_map("model.language_model.layers.33.mlp.up_proj.weight"))
        self.assertEqual(key_map("model.language_model.layers.32.mlp.up_proj.weight"), "layers.32.mlp.up_proj.weight")
        self.assertEqual(key_map("model.language_model.norm.weight"), "norm.weight")
        self.assertEqual(key_map("lm_head.weight"), "lm_head.weight")

    def test_vl_mode(self) -> None:
        key_map = te.vl_mode_key_map(33)
        self.assertEqual(key_map("model.visual.blocks.0.attn.qkv.weight"), "visual.blocks.0.attn.qkv.weight")
        self.assertIsNone(key_map("model.language_model.layers.35.mlp.up_proj.weight"))
        self.assertEqual(
            key_map("model.language_model.embed_tokens.weight"), "language_model.embed_tokens.weight"
        )
        self.assertEqual(key_map("lm_head.weight"), "lm_head.weight")


class PaddingRuleTests(unittest.TestCase):
    def test_padded_length(self) -> None:
        cases = {1: 80, 79: 80, 80: 80, 81: 160, 4000: 4000, 4001: 4080, 4081: 4096, 4096: 4096}
        for real, expected in cases.items():
            self.assertEqual(te.padded_length(real), expected, real)

    def test_padded_length_refuses_empty(self) -> None:
        with self.assertRaises(ValueError):
            te.padded_length(0)

    def test_reference_position_ids(self) -> None:
        mask = torch.tensor([[1, 1, 1, 0, 0]])
        ids = te.reference_position_ids(mask)
        self.assertEqual(ids.shape, (4, 1, 5))
        for row in ids:
            self.assertEqual(row[0].tolist(), [0, 1, 2, 1, 1])

    def test_right_pad(self) -> None:
        ids, mask = te.right_pad(torch.tensor([[5, 6, 7]]), length=6, pad_id=_PAD)
        self.assertEqual(ids.tolist(), [[5, 6, 7, _PAD, _PAD, _PAD]])
        self.assertEqual(mask.tolist(), [[1, 1, 1, 0, 0, 0]])

    def test_right_pad_never_truncates(self) -> None:
        with self.assertRaises(ValueError):
            te.right_pad(torch.zeros(1, 7, dtype=torch.long), length=6, pad_id=_PAD)


class VlBudgetTests(unittest.TestCase):
    def test_the_user_cap_wins_when_there_is_room(self) -> None:
        self.assertEqual(te.vl_pixel_budget(20, 1, vl_ref_max_pixels=512 * 512), 512 * 512)

    def test_the_sequence_cap_is_shared_equally(self) -> None:
        budget = te.vl_pixel_budget(20, 3, vl_ref_max_pixels=10**9)
        self.assertEqual(budget, (4096 - 20 - 6) // 3 * 1024)
        self.assertLessEqual(20 + 3 * (2 + budget // te.VL_PIXELS_PER_TOKEN), TEXT_MAX_TOKENS)

    def test_the_smallest_budget_is_accepted(self) -> None:
        self.assertEqual(te.vl_pixel_budget(4090, 1, vl_ref_max_pixels=10**9), 4 * 1024)

    def test_refusals(self) -> None:
        with self.assertRaisesRegex(ValueError, "слишком длинный"):
            te.vl_pixel_budget(4091, 1, vl_ref_max_pixels=10**9)
        with self.assertRaisesRegex(ValueError, "слишком длинный"):
            te.vl_pixel_budget(5000, 2, vl_ref_max_pixels=10**9)
        with self.assertRaisesRegex(ValueError, "меньше допустимого минимума"):
            te.vl_pixel_budget(20, 1, vl_ref_max_pixels=1000)
        with self.assertRaisesRegex(ValueError, "хотя бы одно"):
            te.vl_pixel_budget(20, 0, vl_ref_max_pixels=10**9)

    def test_encode_with_references_needs_a_vl_handle(self) -> None:
        handle = _handle(None, EncoderMode.TEXT)
        with self.assertRaisesRegex(ValueError, "режиме VL"):
            te.encode_with_references(handle, "a cat", [], vl_ref_max_pixels=512 * 512)


if __name__ == "__main__":
    unittest.main()
