"""
File: modules/ai_backend/runtime/test_qwen3_gguf.py

Purpose:
Unit tests for `runtime/qwen3_gguf.py`, the llama.cpp GGUF <-> HF Qwen3 name
bridge: full 398-name coverage of a real Qwen3-4B name set, truncation, the
dropped head, refusals (with the caller's owner name and logger), the
`general.architecture` check for one or several accepted ids, and the fact
that lets a Qwen3-VL text encoder use the bridge: `Qwen3VLTextModel` has
exactly the `Qwen3Model` state-key set.

Main responsibilities:
- `KeyMapTests` - the name map (moved from FLUX.2 klein's `test_encoder_gguf.py`
  unchanged, apart from the owner / logger keywords);
- `RequireArchitectureTests` - acceptance and the exact refusal wording;
- `Qwen3VlTextKeySetTests` - the key-set equality on tiny `meta` models;
- `RealQwen3GgufHeaderTests` - opt-in, header only: every real Qwen3 / Qwen3-VL
  GGUF text encoder on this machine has an accepted architecture and maps
  completely onto the full-depth `Qwen3Model` key set.

Notes:
- Needs gguf, torch, transformers and accelerate (each class skips cleanly
  without them). No GPU, no network, writes nothing.
- Real files: `<side_models>/{FLUX.2-klein-4B,FLUX.2-klein-9B}/{text_encoder_gguf,
  text_encoder_uncensored_gguf}/*.gguf`, `<side_models>/FLUX.3-image-klein/text_encoder_gguf/*.gguf`,
  plus the files named by `MS_QWEN3_GGUF_FILES` (an `os.pathsep`-separated
  list). Without any, the real-file test skips. Only headers are read.
"""

from __future__ import annotations

import logging
import os
import unittest
from pathlib import Path

from . import qwen3_gguf
from .paths import side_models_root


def _has(*modules: str) -> bool:
    try:
        for name in modules:
            __import__(name)
    except ImportError:
        return False
    return True


_NAME_STACK = ("gguf", "torch", "transformers", "accelerate")

#: The decoder layers FLUX.2 klein keeps of its Qwen3 encoder (`components.ENCODER_KEEP_LAYERS`).
_KEEP = 28

#: The owner name and logger the refusal tests pass (any service would do).
_OWNER = "Test owner"
_LOG = logging.getLogger(__name__)

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


def _require_known(names: list[str], keep: int) -> None:
    qwen3_gguf.require_known_encoder_names(names, keep, source=Path("x.gguf"), owner=_OWNER, logger=_LOG)


@unittest.skipUnless(_has(*_NAME_STACK), "needs gguf, torch, transformers and accelerate")
class KeyMapTests(unittest.TestCase):
    def test_every_name_of_a_real_file_maps_or_is_skipped(self) -> None:
        names = _qwen3_4b_names()
        self.assertEqual(len(names), 398)
        mapped = {name: qwen3_gguf.encoder_state_key_of_gguf(name, _KEEP) for name in names}
        skipped = [name for name, key in mapped.items() if key is None]
        self.assertEqual(len(skipped), (36 - _KEEP) * len(_LAYER_TENSORS), "exactly the blocks past the truncation")
        self.assertTrue(all(int(name.split(".")[1]) >= _KEEP for name in skipped))
        keys = [key for key in mapped.values() if key is not None]
        self.assertEqual(len(keys), len(set(keys)), "no two tensors share a state key")
        self.assertEqual(set(keys), qwen3_gguf._expected_state_keys(_KEEP))
        _require_known(names, _KEEP)

    def test_recorded_pairs_and_no_renaming_of_rows(self) -> None:
        for name, key in _RECORDED_PAIRS.items():
            with self.subTest(name=name):
                self.assertEqual(qwen3_gguf.encoder_state_key_of_gguf(name, 28), key)

    def test_the_head_and_the_truncation_are_dropped(self) -> None:
        self.assertIsNone(qwen3_gguf.encoder_state_key_of_gguf("output.weight", 28))
        self.assertIsNone(qwen3_gguf.encoder_state_key_of_gguf("blk.28.attn_q.weight", 28))
        self.assertEqual(qwen3_gguf.encoder_state_key_of_gguf("blk.28.attn_q.weight", 30), "layers.28.self_attn.q_proj.weight")

    def test_unknown_names_are_refused(self) -> None:
        for name in ("rope_freqs.weight", "blk.0.attn_q.bias", "blk.0.attn_qkv.weight", "foo.weight"):
            with self.subTest(name=name), self.assertRaises(ValueError):
                qwen3_gguf.encoder_state_key_of_gguf(name, 28)
        with self.assertRaisesRegex(ValueError, "rope_freqs.weight"):
            _require_known([*_qwen3_4b_names(), "rope_freqs.weight"], 28)

    def test_a_file_missing_kept_weights_is_refused(self) -> None:
        names = [name for name in _qwen3_4b_names() if name != "blk.5.ffn_up.weight"]
        with self.assertRaisesRegex(ValueError, "layers.5.mlp.up_proj.weight"):
            _require_known(names, 28)
        # A missing tensor past the truncation is not needed at all.
        names = [name for name in _qwen3_4b_names() if name != "blk.30.ffn_up.weight"]
        _require_known(names, 28)

    def test_the_name_spaces_are_disjoint(self) -> None:
        for name in ("blk.0.attn_q.weight", "token_embd.weight", "output_norm.weight", "output.weight"):
            self.assertTrue(qwen3_gguf.is_gguf_encoder_name(name), name)
        for name in ("model.layers.0.self_attn.q_proj.weight", "lm_head.weight", "layers.0.mlp.up_proj.weight", "embed_tokens.weight", "norm.weight"):
            self.assertFalse(qwen3_gguf.is_gguf_encoder_name(name), name)

    def test_refusals_name_the_owner_and_log_on_the_callers_logger(self) -> None:
        with self.assertLogs(_LOG, level=logging.ERROR) as captured, self.assertRaises(ValueError) as caught:
            _require_known([*_qwen3_4b_names(), "rope_freqs.weight"], 28)
        self.assertIn(f"текстового энкодера {_OWNER} ", str(caught.exception))
        self.assertEqual(len(captured.records), 1)
        self.assertTrue(captured.records[0].getMessage().startswith(f"{_OWNER}: "))
        names = [name for name in _qwen3_4b_names() if name != "blk.5.ffn_up.weight"]
        with self.assertLogs(_LOG, level=logging.ERROR) as captured, self.assertRaises(ValueError):
            _require_known(names, 28)
        self.assertTrue(captured.records[0].getMessage().startswith(f"{_OWNER}: "))


class RequireArchitectureTests(unittest.TestCase):
    """Pure mapping checks: no gguf package needed."""

    def test_an_accepted_architecture_passes(self) -> None:
        for architecture, accepted in (("qwen3", {"qwen3"}), ("qwen3vl", {"qwen3vl"}), ("qwen3", {"qwen3", "qwen3vl"}), ("qwen3vl", {"qwen3", "qwen3vl"})):
            with self.subTest(architecture=architecture, accepted=sorted(accepted)):
                qwen3_gguf.require_architecture({"general.architecture": architecture}, frozenset(accepted), source=Path("e.gguf"), owner=_OWNER)

    def test_one_accepted_id_gives_the_single_id_wording(self) -> None:
        with self.assertRaises(ValueError) as caught:
            qwen3_gguf.require_architecture({"general.architecture": "llama"}, frozenset({"qwen3"}), source=Path("/x/llama.gguf"), owner="FLUX.2 klein")
        self.assertEqual(
            str(caught.exception),
            "Текстовый энкодер llama.gguf: это GGUF модели «llama», а нужен Qwen3 "
            "(general.architecture = «qwen3») — возьмите GGUF текстового энкодера FLUX.2 klein.",
        )

    def test_several_accepted_ids_are_listed_in_order(self) -> None:
        with self.assertRaises(ValueError) as caught:
            qwen3_gguf.require_architecture({"general.architecture": "qwen3"}, frozenset({"qwen3vl"}), source=Path("q.gguf"), owner=_OWNER)
        self.assertIn("(general.architecture = «qwen3vl»)", str(caught.exception))
        with self.assertRaises(ValueError) as caught:
            qwen3_gguf.require_architecture({"general.architecture": "llama"}, frozenset({"qwen3vl", "qwen3"}), source=Path("q.gguf"), owner=_OWNER)
        self.assertIn("(general.architecture = «qwen3» / «qwen3vl»)", str(caught.exception))

    def test_a_file_without_metadata_is_refused(self) -> None:
        with self.assertRaisesRegex(ValueError, "«None»"):
            qwen3_gguf.require_architecture({}, frozenset({"qwen3"}), source=Path("q.gguf"), owner=_OWNER)


@unittest.skipUnless(_has(*_NAME_STACK), "needs gguf, torch, transformers and accelerate")
class Qwen3VlTextKeySetTests(unittest.TestCase):
    def test_the_qwen3_vl_text_model_has_the_qwen3_model_key_set(self) -> None:
        from accelerate import init_empty_weights
        from transformers import Qwen3Config, Qwen3Model, Qwen3VLTextConfig, Qwen3VLTextModel

        sizes = {"vocab_size": 32, "hidden_size": 32, "intermediate_size": 32, "num_hidden_layers": 2, "num_attention_heads": 2, "num_key_value_heads": 1, "head_dim": 16}
        # Qwen3-VL's text rotary embedding is interleaved M-RoPE: its config
        # must carry an `mrope_section` summing to `head_dim / 2`.
        vl_config = Qwen3VLTextConfig(**sizes, rope_scaling={"rope_type": "default", "mrope_section": [4, 2, 2]})
        with init_empty_weights():
            qwen3 = set(Qwen3Model(Qwen3Config(**sizes)).state_dict())
            qwen3_vl = set(Qwen3VLTextModel(vl_config).state_dict())
        self.assertEqual(qwen3_vl, qwen3)
        self.assertEqual(qwen3, qwen3_gguf._expected_state_keys(2), "the bridge's key set is the model's")


#: Folders holding real Qwen3 / Qwen3-VL GGUF text encoders (module notes).
_REAL_FOLDERS = [
    *(side_models_root() / package / folder for package in ("FLUX.2-klein-4B", "FLUX.2-klein-9B") for folder in ("text_encoder_gguf", "text_encoder_uncensored_gguf")),
    side_models_root() / "FLUX.3-image-klein" / "text_encoder_gguf",
]

#: Environment variable naming extra real GGUF encoder files to check.
_FILES_ENV = "MS_QWEN3_GGUF_FILES"

#: The architectures a Qwen3 decoder stack ships under in llama.cpp.
_ACCEPTED = frozenset({"qwen3", "qwen3vl"})


def _real_files() -> list[Path]:
    found = [path for folder in _REAL_FOLDERS if folder.is_dir() for path in sorted(folder.glob("*.gguf"))]
    extra = [Path(item) for item in os.environ.get(_FILES_ENV, "").split(os.pathsep) if item.strip()]
    return found + [path for path in extra if path.is_file()]


@unittest.skipUnless(_has(*_NAME_STACK), "needs gguf, torch, transformers and accelerate")
class RealQwen3GgufHeaderTests(unittest.TestCase):
    def test_every_real_file_maps_completely_at_full_depth(self) -> None:
        from .tensor_container import read_container_layout

        files = _real_files()
        if not files:
            self.skipTest(f"no Qwen3 GGUF text encoder on this machine ({', '.join(str(folder) for folder in _REAL_FOLDERS)}, or set {_FILES_ENV})")
        for path in files:
            with self.subTest(file=path.name):
                layout = read_container_layout(path)
                self.assertIsNotNone(layout.gguf, "a GGUF file")
                metadata = layout.gguf.metadata  # type: ignore[union-attr]
                qwen3_gguf.require_architecture(metadata, _ACCEPTED, source=path, owner=_OWNER)
                depth = int(metadata[f"{metadata['general.architecture']}.block_count"])
                names = [span.name for span in layout.spans]
                qwen3_gguf.require_known_encoder_names(names, depth, source=path, owner=_OWNER, logger=_LOG)
                keys = [key for key in (qwen3_gguf.encoder_state_key_of_gguf(name, depth) for name in names) if key is not None]
                self.assertEqual(len(keys), len(set(keys)), "no two tensors share a state key")
                self.assertEqual(set(keys), qwen3_gguf._expected_state_keys(depth))
                self.assertTrue(qwen3_gguf.gguf_encoder_quant_types(layout.spans, depth), "a quantized file has quantized tensors")


if __name__ == "__main__":
    unittest.main()
