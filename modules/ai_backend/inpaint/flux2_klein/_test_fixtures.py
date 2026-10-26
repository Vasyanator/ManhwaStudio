"""
File: modules/ai_backend/inpaint/flux2_klein/_test_fixtures.py

Purpose:
Shared fixtures for the FLUX.2 klein test modules: the on-disk model tree the
path-based tests need (header-only weights by default, tiny REAL weights on
request) and `_ManagedRunFixture`, a service over a REAL large-model manager on
the CPU.

Key declarations:
- `_make_model_tree` / `_write_header_only` / `_write_real_weights` /
  `_write_safetensors` / `_png_bytes` - on-disk inputs (`_write_safetensors`
  writes a raw header dict, for malformed / fp8 containers).
- `VAE_SAMPLE_SIZE` / `VAE_BLOCK_OUT_CHANNELS` and the thresholds derived from
  them - the fixture VAE's tiling geometry, written into its `config.json` and
  DERIVED exactly as diffusers derives them (never set independently).
- `TINY_TRANSFORMER_CONFIG` / `TINY_VAE_CONFIG` / `TINY_ENCODER_CONFIG` - the tiny
  real models (the encoder has exactly `ENCODER_KEEP_LAYERS` layers and a width
  that matches the transformer, so the variant guard passes).
- `_write_tiny_gguf` - a minimal GGUF transformer file (header-readable, BFL
  names) for the format / params / refusal tests;
  `TINY_GGUF_ENCODER_CONFIG` / `_write_qwen3_gguf_encoder` - a tiny REAL
  Qwen3 encoder written as a llama.cpp GGUF (quantized linears and
  embedding, F32 norms, `qwen3.*` metadata) plus its truncated reference
  state;
  `_write_bfl_gguf_transformer` - the tiny REAL transformer re-written as a
  GGUF in the BFL layout (fused qkv, swapped final adaLN, block-quantized
  linears) plus the diffusers state dict it must load as.
- `DEFAULT_MODEL_INDEX` - the `model_index.json` the tree carries by default, kept
  faithful to the shipped klein 4B one (`is_distilled: true`). Pass another dict,
  or `None` for no manifest at all, to build an undeclared checkpoint.
- `_TempTreeCase` - a `TestCase` with a complete model tree in a temp directory
  (`real_weights = True` in a subclass for the tiny real models); it points
  `components.installed_klein_package_roots` at `<tmp>/_installed/`, where
  the packages `self.installed_package` (4B) and `self.installed_package_9b`
  are absent until a test creates them, so the GGUF package fallback never
  sees the machine's real `side_models/`.
- `FIXTURES_DIR`, `load_9b_header_fixture`, `load_9b_config` and
  `write_header_only_gguf` - the recorded FLUX.2 klein 9B GGUF headers and
  configs (`fixtures/flux2_9b_*.json`, no tensor data) and a stdlib writer that
  turns one recorded header into a header-only GGUF file (a header view
  needs no tensor bytes: `runtime.tensor_container`).
- `_FakeEmbeds` - a prompt-embedding stand-in for the library tests.
- `_ManagedRunFixture` (+ `_RoomyProbes`, `_FakeRunPipeline`, `_ImageProcessor`,
  `_flat_tables`) - real manager, arbiter, small pool, torch and model classes;
  fake pipeline call, tokenizer, budget probes and transport device.

Notes:
- The leading underscore keeps pytest from collecting this file as a test module.
- Module attributes are patched on the module that DEFINES them (`hardware`,
  `memory`, ...), never on the `flux2_klein` package: the package's re-export is
  a separate binding, so patching it would not reach the real callers.
"""

from __future__ import annotations

import json
import struct
import tempfile
import types
import unittest
from pathlib import Path
from unittest.mock import patch

import numpy as np
from PIL import Image

from modules.ai_backend.inpaint import flux2_klein as svc
from modules.ai_backend.inpaint.flux2_klein import components, hardware
from modules.ai_backend.runtime.model_manager import SmallModelManager


# ---------------------------------------------------------------------------
# Fixtures shared by several test classes
# ---------------------------------------------------------------------------
#: `sample_size` and `block_out_channels` written into the fixture VAE config and
#: mirrored by `_make_fake_vae`. They are NOT two independent knobs: diffusers
#: derives `tile_sample_min_size = sample_size` and
#: `tile_latent_min_size = int(sample_size / 2 ** (len(block_out_channels) - 1))`
#: in `AutoencoderKLFlux2.__init__`, so a fixture that sets them separately would
#: model a VAE that cannot exist. These are the shipped klein VAE's values.
VAE_SAMPLE_SIZE = 1024
VAE_BLOCK_OUT_CHANNELS = (128, 256, 512, 512)

#: Pixels per latent cell for that VAE, and the two thresholds it implies.
VAE_LATENT_SCALE = 2 ** (len(VAE_BLOCK_OUT_CHANNELS) - 1)
VAE_TILE_LATENT_MIN_SIZE = int(VAE_SAMPLE_SIZE / VAE_LATENT_SCALE)


#: `model_index.json` written into the fixture tree by default, trimmed from the
#: shipped `side_models/FLUX.2-klein-4B/model_index.json` to the fields this
#: backend reads. The real checkpoint declares `is_distilled: true`, so the
#: default fixture must too — a fixture that quietly declared `false` would let a
#: regression in the guidance skip pass every test.
DEFAULT_MODEL_INDEX: dict[str, object] = {
    "_class_name": "Flux2KleinPipeline",
    "is_distilled": True,
}


#: Header-only tensors of the default tree (`(dtype, shape)`, no data bytes):
#: valid containers the manager's inventory can read, in each component's
#: real key layout (diffusers transformer, Qwen3 causal-LM encoder, diffusers VAE).
_HEADER_ONLY_TENSORS: dict[str, dict[str, tuple[str, tuple[int, ...]]]] = {
    "transformer": {"x_embedder.weight": ("BF16", (8, 8)), "transformer_blocks.0.attn.to_q.weight": ("BF16", (8, 8)), "single_transformer_blocks.0.attn.to_out.weight": ("BF16", (8, 8))},
    "text_encoder": {"model.embed_tokens.weight": ("BF16", (16, 8)), "model.layers.0.self_attn.q_proj.weight": ("BF16", (8, 8)), "model.norm.weight": ("BF16", (8,)), "lm_head.weight": ("BF16", (16, 8))},
    "vae": {"decoder.conv_in.weight": ("BF16", (4, 4))},
}


def _write_header_only(path: Path, tensors: dict[str, tuple[str, tuple[int, ...]]]) -> None:
    """A valid safetensors HEADER for `tensors` (back to back offsets) with no data bytes."""
    item = {"BF16": 2, "F16": 2, "F32": 4, "I64": 8}
    header: dict[str, object] = {}
    offset = 0
    for name, (dtype, shape) in tensors.items():
        length = item[dtype]
        for side in shape:
            length *= side
        header[name] = {"dtype": dtype, "shape": list(shape), "data_offsets": [offset, offset + length]}
        offset += length
    payload = json.dumps(header).encode("utf-8")
    path.write_bytes(struct.pack("<Q", len(payload)) + payload)


def _make_model_tree(
    root: Path,
    *,
    single_file_transformer: bool = False,
    model_index: dict[str, object] | None = DEFAULT_MODEL_INDEX,
    real_weights: bool = False,
) -> dict[str, str]:
    """Lay out a klein-like checkout and return the three user-supplied paths.

    `model_index` is the manifest written at the checkout root; pass `None` to
    build a tree with no `model_index.json` at all, which is what a
    hand-assembled folder of components looks like. The weights are
    header-only containers by default (enough for every header reader and the
    manager's inventory); `real_weights=True` (needs torch, diffusers and
    transformers) writes tiny REAL models with their configs instead
    (`_write_real_weights`), which the manager can load and run on the CPU.
    """
    if model_index is not None:
        root.mkdir(parents=True, exist_ok=True)
        (root / "model_index.json").write_text(json.dumps(model_index), encoding="utf-8")
    (root / "text_encoder").mkdir(parents=True)
    (root / "text_encoder" / "config.json").write_text("{}", encoding="utf-8")
    _write_header_only(root / "text_encoder" / "model.safetensors", _HEADER_ONLY_TENSORS["text_encoder"])
    (root / "vae").mkdir()
    # The two fields the tiling threshold is derived from, at the shipped klein
    # VAE's values: `sample_size` 1024 over four `block_out_channels` gives a
    # latent threshold of 128 and therefore a 1024 px side threshold, which is
    # what `components.vae_tile_threshold_pixels` must answer. A `{}` config
    # would send every test down the unreadable-config fallback instead.
    (root / "vae" / "config.json").write_text(
        json.dumps({"sample_size": VAE_SAMPLE_SIZE, "block_out_channels": list(VAE_BLOCK_OUT_CHANNELS)}),
        encoding="utf-8",
    )
    _write_header_only(root / "vae" / "diffusion_pytorch_model.safetensors", _HEADER_ONLY_TENSORS["vae"])
    (root / "tokenizer").mkdir()
    (root / "tokenizer" / "tokenizer_config.json").write_text("{}", encoding="utf-8")
    (root / "scheduler").mkdir()
    (root / "scheduler" / "scheduler_config.json").write_text("{}", encoding="utf-8")

    if single_file_transformer:
        # The layout a klein standalone release actually ships: the checkpoint in
        # the repository root and its own `transformer/config.json` beside it.
        # Without that config the load is refused, so it belongs to the fixture.
        transformer = root / "flux2-klein.safetensors"
        _write_safetensors(transformer, {"single_stream_modulation.lin.weight": {"dtype": "BF16"}})
        (root / "transformer").mkdir()
        (root / "transformer" / "config.json").write_text(
            json.dumps({"_class_name": "Flux2Transformer2DModel"}), encoding="utf-8"
        )
    else:
        transformer = root / "transformer"
        transformer.mkdir()
        (transformer / "config.json").write_text("{}", encoding="utf-8")
        _write_header_only(transformer / "diffusion_pytorch_model.safetensors", _HEADER_ONLY_TENSORS["transformer"])

    if real_weights:
        if single_file_transformer:
            raise ValueError("real_weights: the tiny real tree is a diffusers folder layout")
        _write_real_weights(root)
    return {
        "text_encoder_path": str(root / "text_encoder"),
        "transformer_path": str(transformer),
        "vae_path": str(root / "vae"),
    }


#: Tiny real configs of `_write_real_weights` (CPU-fast; the VAE keeps the
#: shipped tiling geometry: four `block_out_channels`, `sample_size` 1024).
TINY_TRANSFORMER_CONFIG: dict[str, object] = {
    "_class_name": "Flux2Transformer2DModel",
    "in_channels": 8,
    "num_layers": 1,
    "num_single_layers": 2,
    "attention_head_dim": 16,
    "num_attention_heads": 2,
    "joint_attention_dim": 24,
    "axes_dims_rope": [4, 4, 4, 4],
    "timestep_guidance_channels": 32,
    "guidance_embeds": False,
}
TINY_VAE_CONFIG: dict[str, object] = {
    "_class_name": "AutoencoderKLFlux2",
    "block_out_channels": [8, 8, 8, 8],
    "latent_channels": 4,
    "norm_num_groups": 4,
    "layers_per_block": 1,
    "sample_size": VAE_SAMPLE_SIZE,
}
#: 28 decoder layers: exactly `ENCODER_KEEP_LAYERS`, the minimum the pipeline reads.
TINY_ENCODER_CONFIG: dict[str, object] = {
    "architectures": ["Qwen3ForCausalLM"],
    "model_type": "qwen3",
    # 3 x 8 = the tiny transformer's `joint_attention_dim` (the variant guard).
    "hidden_size": 8,
    "num_attention_heads": 2,
    "num_key_value_heads": 1,
    "head_dim": 4,
    "intermediate_size": 16,
    "num_hidden_layers": 28,
    "vocab_size": 64,
}


def _write_real_weights(root: Path) -> None:
    """Overwrite the tree's three weight files and configs with tiny REAL models (seeded, CPU).

    The transformer is written in the diffusers layout, the encoder as a
    causal-LM checkpoint (`model.` prefix plus an `lm_head`) the way the
    shipped one is, the VAE in its folder.
    """
    import torch
    from diffusers import AutoencoderKLFlux2, Flux2Transformer2DModel
    from safetensors.torch import save_file
    from transformers import Qwen3Config, Qwen3Model

    torch.manual_seed(0)
    transformer = Flux2Transformer2DModel.from_config({key: value for key, value in TINY_TRANSFORMER_CONFIG.items() if key != "_class_name"})
    save_file({key: value.to(torch.bfloat16).contiguous() for key, value in transformer.state_dict().items()}, str(root / "transformer" / "diffusion_pytorch_model.safetensors"))
    (root / "transformer" / "config.json").write_text(json.dumps(TINY_TRANSFORMER_CONFIG), encoding="utf-8")
    vae = AutoencoderKLFlux2.from_config({key: value for key, value in TINY_VAE_CONFIG.items() if key != "_class_name"})
    save_file({key: (value.to(torch.bfloat16) if value.is_floating_point() else value).contiguous() for key, value in vae.state_dict().items()}, str(root / "vae" / "diffusion_pytorch_model.safetensors"))
    (root / "vae" / "config.json").write_text(json.dumps(TINY_VAE_CONFIG), encoding="utf-8")
    encoder = Qwen3Model(Qwen3Config(**{key: value for key, value in TINY_ENCODER_CONFIG.items() if key not in ("architectures", "model_type")}))
    tensors = {f"model.{key}": value.to(torch.float16).contiguous() for key, value in encoder.state_dict().items()}
    tensors["lm_head.weight"] = torch.zeros((int(TINY_ENCODER_CONFIG["vocab_size"]), int(TINY_ENCODER_CONFIG["hidden_size"])), dtype=torch.float16)
    save_file(tensors, str(root / "text_encoder" / "model.safetensors"))
    (root / "text_encoder" / "config.json").write_text(json.dumps(TINY_ENCODER_CONFIG), encoding="utf-8")


def _write_tiny_gguf(path: Path, *, quant: str = "Q8_0") -> Path:
    """A minimal GGUF transformer file at `path` (parents created): a block-quantized BFL qkv in `quant`, a BF16 embedder, an F32 norm.

    Header-readable only (no model matches it); needs the `gguf` package.
    """
    from modules.ai_backend.runtime._gguf_test_fixtures import Q, make_tensor, write_gguf

    path.parent.mkdir(parents=True, exist_ok=True)
    tensors = [
        make_tensor("img_in.weight", Q.BF16, (32, 8), seed=1),
        make_tensor("double_blocks.0.img_attn.qkv.weight", Q[quant], (96, 256 if quant.endswith("_K") else 32), seed=2),
        make_tensor("double_blocks.0.img_attn.norm.query_norm.scale", Q.F32, (16,), seed=3),
    ]
    return write_gguf(path, tensors)


#: diffusers -> BFL renames of the tiny transformer (the inverse of the
#: converter's renames; the fused qkv and the final adaLN are built apart).
_DIFFUSERS_TO_BFL = (
    ("time_guidance_embed.timestep_embedder.linear_1.", "time_in.in_layer."),
    ("time_guidance_embed.timestep_embedder.linear_2.", "time_in.out_layer."),
    ("double_stream_modulation_img.linear.", "double_stream_modulation_img.lin."),
    ("double_stream_modulation_txt.linear.", "double_stream_modulation_txt.lin."),
    ("single_stream_modulation.linear.", "single_stream_modulation.lin."),
    ("x_embedder.", "img_in."),
    ("context_embedder.", "txt_in."),
    ("proj_out.", "final_layer.linear."),
)
_DOUBLE_TO_BFL = {
    "attn.norm_q.weight": "img_attn.norm.query_norm.scale",
    "attn.norm_k.weight": "img_attn.norm.key_norm.scale",
    "attn.norm_added_q.weight": "txt_attn.norm.query_norm.scale",
    "attn.norm_added_k.weight": "txt_attn.norm.key_norm.scale",
    "attn.to_out.0.weight": "img_attn.proj.weight",
    "attn.to_add_out.weight": "txt_attn.proj.weight",
    "ff.linear_in.weight": "img_mlp.0.weight",
    "ff.linear_out.weight": "img_mlp.2.weight",
    "ff_context.linear_in.weight": "txt_mlp.0.weight",
    "ff_context.linear_out.weight": "txt_mlp.2.weight",
}
_SINGLE_TO_BFL = {
    "attn.to_qkv_mlp_proj.weight": "linear1.weight",
    "attn.to_out.weight": "linear2.weight",
    "attn.norm_q.weight": "norm.query_norm.scale",
    "attn.norm_k.weight": "norm.key_norm.scale",
}


def _bfl_tensors_of(diffusers_state: dict[str, object]) -> dict[str, object]:
    """The BFL-layout tensors of a diffusers FLUX.2 state dict (test inverse of diffusers' converter, torch tensors in, torch tensors out)."""
    import torch

    out: dict[str, object] = {}
    fused: dict[str, list[object]] = {}
    for key, value in diffusers_state.items():
        parts = key.split(".")
        if parts[0] == "transformer_blocks":
            index, inner = parts[1], ".".join(parts[2:])
            for stream, names in (("img_attn", ("attn.to_q.weight", "attn.to_k.weight", "attn.to_v.weight")), ("txt_attn", ("attn.add_q_proj.weight", "attn.add_k_proj.weight", "attn.add_v_proj.weight"))):
                if inner in names:
                    fused.setdefault(f"double_blocks.{index}.{stream}.qkv.weight", [None, None, None])[names.index(inner)] = value
                    break
            else:
                out[f"double_blocks.{index}.{_DOUBLE_TO_BFL[inner]}"] = value
        elif parts[0] == "single_transformer_blocks":
            out[f"single_blocks.{parts[1]}.{_SINGLE_TO_BFL['.'.join(parts[2:])]}"] = value
        elif key == "norm_out.linear.weight":
            # BFL keeps (shift, scale); diffusers (scale, shift): the swap is its own inverse.
            scale, shift = value.chunk(2, dim=0)  # type: ignore[attr-defined]
            out["final_layer.adaLN_modulation.1.weight"] = torch.cat([shift, scale], dim=0)
        else:
            renamed = next((new + key[len(old):] for old, new in _DIFFUSERS_TO_BFL if key.startswith(old)), None)
            if renamed is None:
                raise KeyError(f"no BFL name for {key}")
            out[renamed] = value
    for name, (q, k, v) in fused.items():
        out[name] = torch.cat([q, k, v], dim=0)  # type: ignore[list-item]
    return out


def _write_bfl_gguf_transformer(root: Path, path: Path, *, quant: str = "Q8_0") -> dict[str, object]:
    """Re-write the tree's tiny REAL transformer (`_write_real_weights`) as a BFL-layout GGUF at `path`; return the diffusers state dict it must load as.

    Every 2-D weight whose row is whole blocks of `quant` (block 32: Q8_0 /
    Q4_0 / Q5_0) is quantized from its bf16 values — the fused qkv and the
    swapped final adaLN included — the other 2-D weights stay BF16 and the
    norm scales F32. The returned reference (bf16, diffusers keys) is the
    file's values dequantized by `runtime.gguf_layers.dequantize`, then put
    through diffusers' own converter on FLOAT tensors: a loader that splits
    and swaps the quantized ROWS must reproduce it bit for bit.
    """
    import numpy as np
    import torch
    from gguf import quants

    from modules.ai_backend.runtime._gguf_test_fixtures import FixtureTensor, Q, write_gguf
    from modules.ai_backend.runtime.gguf_layers import dequantize
    from modules.ai_backend.runtime.safetensors_stream import StreamingSafetensorsReader

    from . import conversion

    source = root / "transformer" / "diffusion_pytorch_model.safetensors"
    with StreamingSafetensorsReader(source) as reader:
        diffusers_state = {span.name: reader.read_tensor(span).clone() for span in reader.spans}
    qtype = Q[quant]
    fixtures: list[FixtureTensor] = []
    reference: dict[str, object] = {}
    for name, value in _bfl_tensors_of(diffusers_state).items():
        if value.dim() == 1:  # type: ignore[attr-defined]
            values = value.float().numpy()  # type: ignore[attr-defined]
            fixtures.append(FixtureTensor(name=name, qtype=Q.F32, shape=tuple(values.shape), raw=np.ascontiguousarray(values)))
            restored = torch.from_numpy(values.copy()).to(torch.bfloat16)
        elif value.shape[-1] % 32 == 0:  # type: ignore[attr-defined]
            raw = np.ascontiguousarray(quants.quantize(value.float().numpy(), qtype))  # type: ignore[attr-defined]
            fixtures.append(FixtureTensor(name=name, qtype=qtype, shape=tuple(value.shape), raw=raw))  # type: ignore[attr-defined]
            restored = dequantize(torch.from_numpy(raw.copy()), int(qtype), row_elements=int(value.shape[-1])).to(torch.bfloat16)  # type: ignore[attr-defined]
        else:
            raw = np.ascontiguousarray(value.contiguous().view(torch.uint8).numpy())  # type: ignore[attr-defined]
            fixtures.append(FixtureTensor(name=name, qtype=Q.BF16, shape=tuple(value.shape), raw=raw))  # type: ignore[attr-defined]
            restored = value.to(torch.bfloat16)  # type: ignore[attr-defined]
        reference.update(conversion.convert_bfl_tensor(name, restored))
    path.parent.mkdir(parents=True, exist_ok=True)
    write_gguf(path, fixtures)
    return reference


#: A tiny real Qwen3 encoder that a GGUF can hold block-quantized: every row is
#: whole Q8_0 blocks (32), 30 layers so the truncation to `ENCODER_KEEP_LAYERS`
#: (28) has blocks to skip. Its width does not match the tiny transformer, so it
#: is for the encoder's own tests, not for a whole run.
TINY_GGUF_ENCODER_CONFIG: dict[str, object] = {
    "architectures": ["Qwen3ForCausalLM"],
    "model_type": "qwen3",
    "hidden_size": 64,
    "num_attention_heads": 2,
    "num_key_value_heads": 1,
    "head_dim": 32,
    "intermediate_size": 64,
    "num_hidden_layers": 30,
    "vocab_size": 128,
    "tie_word_embeddings": True,
}


def _write_qwen3_gguf_encoder(path: Path, *, quant: str = "Q8_0", config: dict[str, object] | None = None, metadata: dict[str, object] | None = None) -> dict[str, object]:
    """Write a tiny REAL Qwen3 encoder (`config`, default `TINY_GGUF_ENCODER_CONFIG`) as a llama.cpp GGUF at `path`; return its truncated reference state.

    Names are llama.cpp's (the `gguf` package's QWEN3 map applied FORWARD to
    the HF names, `model.` prefixed), with no Q/K row permutation (llama.cpp
    has none for qwen3). The linears and `token_embd` are quantized in
    `quant`, the norms stay F32; the metadata carries the `qwen3.*` hyper
    parameters (`metadata` overrides / adds keys). The returned reference is
    `{Qwen3Model state key: bf16 tensor}` for the first
    `ENCODER_KEEP_LAYERS` layers: the file's values dequantized by
    `runtime.gguf_layers.dequantize`, what a loader must reproduce.
    """
    import gguf
    import numpy as np
    import torch
    from gguf import quants
    from transformers import Qwen3Config, Qwen3Model

    from modules.ai_backend.runtime._gguf_test_fixtures import FixtureTensor, Q, write_gguf
    from modules.ai_backend.runtime.gguf_layers import dequantize

    config = dict(config or TINY_GGUF_ENCODER_CONFIG)
    torch.manual_seed(1)
    model = Qwen3Model(Qwen3Config(**{key: value for key, value in config.items() if key not in ("architectures", "model_type")}))
    layers = int(config["num_hidden_layers"])
    name_map = gguf.get_tensor_name_map(gguf.MODEL_ARCH.QWEN3, layers)
    qtype = Q[quant]
    fixtures: list[FixtureTensor] = []
    reference: dict[str, object] = {}
    keep = svc.ENCODER_KEEP_LAYERS
    for key, value in model.state_dict().items():
        name = name_map.get_name(f"model.{key}", try_suffixes=(".weight", ".bias"))
        assert name is not None, key
        if value.dim() == 1:
            values = value.float().numpy()
            fixtures.append(FixtureTensor(name=name, qtype=Q.F32, shape=tuple(values.shape), raw=np.ascontiguousarray(values)))
            restored = torch.from_numpy(values.copy()).to(torch.bfloat16)
        else:
            raw = np.ascontiguousarray(quants.quantize(value.float().numpy(), qtype))
            fixtures.append(FixtureTensor(name=name, qtype=qtype, shape=tuple(value.shape), raw=raw))
            restored = dequantize(torch.from_numpy(raw.copy()), int(qtype), row_elements=int(value.shape[-1])).to(torch.bfloat16)
        if not (key.startswith("layers.") and int(key.split(".")[1]) >= keep):
            reference[key] = restored
    header = {
        "qwen3.block_count": layers,
        "qwen3.embedding_length": int(config["hidden_size"]),
        "qwen3.feed_forward_length": int(config["intermediate_size"]),
        "qwen3.attention.head_count": int(config["num_attention_heads"]),
        "qwen3.attention.head_count_kv": int(config["num_key_value_heads"]),
        "qwen3.attention.key_length": int(config["head_dim"]),
    }
    header.update(metadata or {})
    path.parent.mkdir(parents=True, exist_ok=True)
    write_gguf(path, fixtures, architecture="qwen3", metadata=header)
    return reference


#: The package's recorded fixtures (`forecast_golden.json`, the 9B headers and configs).
FIXTURES_DIR = Path(__file__).resolve().parent / "fixtures"

#: Recorded GGUF headers of the FLUX.2 klein 9B files (no tensor data):
#: `files[<key>]` holds `metadata`, `tensor_count`, `type_histogram` and, for
#: `f9_Q4_K_M.head` (the unsloth 9B transformer) and `unc_q8.head` (the
#: uncensored encoder), `tensors` = `[name, GGUF dims (innermost first), ggml type]`.
NINE_B_HEADERS_FILE = FIXTURES_DIR / "flux2_9b_gguf_headers.json"


def load_9b_header_fixture(key: str) -> dict[str, object]:
    """One recorded 9B GGUF header (`NINE_B_HEADERS_FILE`), e.g. `"f9_Q4_K_M.head"` or `"unc_q8.head"`."""
    files = json.loads(NINE_B_HEADERS_FILE.read_text(encoding="utf-8"))["files"]
    return files[key]


def load_9b_config(name: str) -> dict[str, object]:
    """A recorded 9B `config.json`: `name` is `transformer`, `text_encoder` or `uncensored_text_encoder` (`fixtures/flux2_9b_<name>_config.json`)."""
    return json.loads((FIXTURES_DIR / f"flux2_9b_{name}_config.json").read_text(encoding="utf-8"))


#: GGUF metadata value type ids (the GGUF spec's `gguf_type`).
_GGUF_UINT32, _GGUF_INT64, _GGUF_FLOAT32, _GGUF_BOOL, _GGUF_STRING = 4, 11, 6, 7, 8

#: Default GGUF data alignment (no `general.alignment` key is written).
_GGUF_ALIGNMENT = 32


def write_header_only_gguf(path: Path, metadata: dict[str, object], tensors: list[list[object]]) -> Path:
    """Write a GGUF v3 file at `path` holding only the header: `metadata` scalars and `tensors` infos (`[name, GGUF dims, ggml type]`).

    Offsets are laid out back to back at the default alignment from each
    tensor's true byte size, so the header is exactly what a real file of that
    layout declares; the data section is absent (a header view never reads
    it). Integers are written as u32 when they fit, else i64; floats as f32.
    Needs the `gguf` package (block sizes). Parents are created.
    """
    from gguf.constants import GGML_QUANT_SIZES, GGMLQuantizationType

    def string(value: str) -> bytes:
        raw = value.encode("utf-8")
        return struct.pack("<Q", len(raw)) + raw

    def value_bytes(value: object) -> bytes:
        if isinstance(value, bool):
            return struct.pack("<I?", _GGUF_BOOL, value)
        if isinstance(value, int):
            return struct.pack("<II", _GGUF_UINT32, value) if 0 <= value < 2**32 else struct.pack("<Iq", _GGUF_INT64, value)
        if isinstance(value, float):
            return struct.pack("<If", _GGUF_FLOAT32, value)
        if isinstance(value, str):
            return struct.pack("<I", _GGUF_STRING) + string(value)
        raise TypeError(f"unsupported GGUF metadata value {value!r}")

    out = bytearray(b"GGUF")
    out += struct.pack("<IQQ", 3, len(tensors), len(metadata))
    for key, value in metadata.items():
        out += string(key) + value_bytes(value)
    offset = 0
    for name, dims, type_id in tensors:
        assert isinstance(name, str) and isinstance(dims, list) and isinstance(type_id, int)
        block_size, type_size = GGML_QUANT_SIZES[GGMLQuantizationType(type_id)]
        rows = 1
        for dim in dims[1:]:
            rows *= int(dim)
        nbytes = rows * (int(dims[0]) // block_size) * type_size
        out += string(name) + struct.pack("<I", len(dims)) + b"".join(struct.pack("<Q", int(dim)) for dim in dims) + struct.pack("<IQ", type_id, offset)
        offset += -(-nbytes // _GGUF_ALIGNMENT) * _GGUF_ALIGNMENT
    out += bytes(-len(out) % _GGUF_ALIGNMENT)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(bytes(out))
    return path


def _write_safetensors(path: Path, header: dict[str, object]) -> None:
    """Write a safetensors container carrying only `header` (no tensor bytes)."""
    payload = json.dumps(header).encode("utf-8")
    path.write_bytes(struct.pack("<Q", len(payload)) + payload)


def _png_bytes(array: np.ndarray, mode: str) -> bytes:
    import io

    with io.BytesIO() as buffer:
        Image.fromarray(array, mode).save(buffer, format="PNG")
        return buffer.getvalue()


class _TempTreeCase(unittest.TestCase):
    """Base class giving every test a throwaway klein-like model tree."""

    single_file_transformer = False
    #: Tiny REAL weights instead of header-only files (`_write_real_weights`).
    real_weights = False
    #: Manifest written at the tree's root; `None` builds a tree without one.
    #: Override it in a subclass to exercise a checkpoint that declares
    #: `is_distilled: false` or declares nothing.
    model_index: dict[str, object] | None = DEFAULT_MODEL_INDEX

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        # The installed klein packages a GGUF component falls back to: absent
        # unless a test creates them (the existence filter runs at call time),
        # so no test sees the real `side_models/` of the machine.
        installed = self.root / "_installed"
        self.installed_package = installed / components.KLEIN_PACKAGE_DIR_NAMES[0]
        self.installed_package_9b = installed / components.KLEIN_PACKAGE_DIR_NAMES[1]
        package_patch = patch.object(components, "installed_klein_package_roots", lambda: components.existing_klein_package_roots(installed))
        package_patch.start()
        self.addCleanup(package_patch.stop)
        self.paths = _make_model_tree(
            self.root,
            single_file_transformer=self.single_file_transformer,
            model_index=self.model_index,
            real_weights=self.real_weights,
        )

    def params(self, **overrides: object) -> dict[str, object]:
        merged: dict[str, object] = dict(self.paths)
        merged.update(overrides)
        return merged

    def declare_distilled(self, value: object) -> None:
        """Rewrite the tree's `model_index.json` in place; `None` removes the file.

        `value` is written verbatim as `is_distilled`, so a test can also declare
        a non-boolean and prove it is refused. The manifest is read at call time,
        never cached, so this takes effect for the next request.
        """
        manifest = self.root / "model_index.json"
        if value is None:
            manifest.unlink(missing_ok=True)
            return
        manifest.write_text(
            json.dumps({"_class_name": "Flux2KleinPipeline", "is_distilled": value}),
            encoding="utf-8",
        )


# ---------------------------------------------------------------------------
# Stand-ins
# ---------------------------------------------------------------------------
class _FakeEmbeds:
    """Prompt-embedding stand-in: records every device it was moved to.

    `_encode_prompt_phase` calls `.detach().to("cpu")` positionally and
    `_generate_locked` calls `.to(device=...)` by keyword, so both spellings are
    accepted.
    """

    def __init__(self, text: str = "", text_tokens: int = 512) -> None:
        self.text = text
        #: `(batch, text tokens, width)`: `_text_attention_mask_locked` reads the
        #: text length from `shape[1]`, exactly as from a real tensor.
        self.shape = (1, int(text_tokens), 8)
        self.moves: list[object] = []

    def detach(self) -> "_FakeEmbeds":
        return self

    def to(self, device: object = None, **kwargs: object) -> "_FakeEmbeds":
        self.moves.append(device if device is not None else kwargs.get("device"))
        return self


# ---------------------------------------------------------------------------
# A real run through the large-model manager, on the CPU
# ---------------------------------------------------------------------------
GIB = 1024**3


class _RoomyProbes:
    """Budget probes of an ample machine (64 GiB device, 64 GiB host), so every plan fits."""

    def __init__(self, *, device_total: int = 64 * GIB, host_available: int = 64 * GIB) -> None:
        self.device_total = device_total
        self.host_available = host_available

    def host(self) -> object:
        from modules.ai_backend.runtime.memory_budget import HostBudget

        return HostBudget(total=128 * GIB, available=self.host_available, cgroup_limit=None, cgroup_used=None, rss_anon=1 << 30, rss_shmem=0, reserve=0)

    def device(self, device: str) -> object:
        from modules.ai_backend.runtime.memory_budget import DeviceBudget

        return DeviceBudget(device=device, total=self.device_total, free=self.device_total, torch_allocated=0, torch_reserved=0, non_torch=0, reserve=0, cap=None)


class _FakeRunPipeline:
    """`Flux2KleinInpaintPipeline` stand-in: the diffusers CALL is the only faked part of a managed run.

    It keeps the components it is built with (the manager's real tiny modules),
    returns zero embeddings of `max_sequence_length` tokens from `encode_prompt`
    and, called, zero latents of the region's latent size, which the real tiny
    VAE then decodes (`calls` / `encode_calls` record the kwargs).
    """

    calls: list[dict[str, object]] = []
    encode_calls: list[dict[str, object]] = []

    def __init__(self, **components: object) -> None:
        self.__dict__.update(components)
        self._attention_kwargs = None

    def set_progress_bar_config(self, **_kwargs: object) -> None:
        return None

    def encode_prompt(self, **kwargs: object) -> tuple[object, object]:
        import torch

        type(self).encode_calls.append(kwargs)
        return torch.zeros((1, int(kwargs["max_sequence_length"]), 24)), None

    def __call__(self, **kwargs: object) -> object:
        """Record the call, report every effective step, return zero latents of the region's latent size."""
        import torch

        type(self).calls.append(kwargs)
        self._attention_kwargs = kwargs.get("attention_kwargs")
        steps = svc.effective_steps(int(kwargs["num_inference_steps"]), float(kwargs["strength"]))  # type: ignore[arg-type]
        for step in range(steps):
            kwargs["callback_on_step_end"](self, step, None, {})  # type: ignore[operator]
        height, width = int(kwargs["height"]), int(kwargs["width"])  # type: ignore[arg-type]
        channels = int(TINY_VAE_CONFIG["latent_channels"])  # type: ignore[arg-type]
        return types.SimpleNamespace(images=torch.zeros((1, channels, height // VAE_LATENT_SCALE, width // VAE_LATENT_SCALE)))


class _ImageProcessor:
    """Turns the real VAE's output into a solid PIL image of the decoded size (value `value`)."""

    def __init__(self, value: int = 210) -> None:
        self.value = value

    def postprocess(self, image: object, output_type: str = "pil") -> list[object]:
        assert output_type == "pil"
        height, width = int(image.shape[-2]), int(image.shape[-1])  # type: ignore[attr-defined]
        return [Image.new("RGB", (width, height), (self.value,) * 3)]


class _ManagedRunFixture(_TempTreeCase):
    """A service over a REAL large-model manager on the CPU, with tiny real weights on disk.

    Real: the manager (planner, loader, ledger, moves, hooks), the arbiter, the
    small pool, torch, the diffusers transformer / VAE classes and the
    transformers Qwen3 class. Faked: the diffusers pipeline CALL
    (`_FakeRunPipeline`), the Qwen tokenizer, the budget probes (`_RoomyProbes`)
    and the transport's device (every "cuda:0" unit lives on the CPU). The run
    device label is `cuda:0`.
    """

    real_weights = True

    def setUp(self) -> None:
        try:
            import diffusers  # noqa: F401 - availability probe
            import torch  # noqa: F401 - availability probe
            import transformers  # noqa: F401 - availability probe
        except ImportError as exc:  # pragma: no cover - the backend may run without torch
            self.skipTest(f"needs torch, diffusers and transformers ({exc})")
        super().setUp()
        import diffusers
        import transformers

        from modules.ai_backend.runtime.large_models.loader import TorchTransport
        from modules.ai_backend.runtime.large_models.manager import LargeModelManager, MemoryLimitsStore
        from modules.ai_backend.runtime.residency.arbiter import ResidencyArbiter

        self.pipeline_cls = type("FakeRunPipeline", (_FakeRunPipeline,), {"calls": [], "encode_calls": [], "image_processor": _ImageProcessor()})
        self.tokenizer_loads: list[str] = []
        tokenizer_loads = self.tokenizer_loads

        class _Tokenizer:
            @staticmethod
            def from_pretrained(path: str, **_kwargs: object) -> object:
                tokenizer_loads.append(path)
                return object()

        for module, name, replacement in (
            (diffusers, "Flux2KleinInpaintPipeline", self.pipeline_cls),
            (transformers, "Qwen2TokenizerFast", _Tokenizer),
            (hardware, "_resolve_selected_backend_device", lambda _fallback: "cuda:0"),
            (hardware, "_clear_torch_cache", lambda: None),
        ):
            attr_patch = patch.object(module, name, replacement)
            attr_patch.start()
            self.addCleanup(attr_patch.stop)

        self.probes = _RoomyProbes()
        arbiter = ResidencyArbiter(self.probes, admission_wait_s=0.0)
        self.small = SmallModelManager(arbiter=arbiter)
        self.large = LargeModelManager(
            arbiter,
            limits_store=MemoryLimitsStore(),
            tables=lambda _device: _flat_tables(),
            probes=self.probes,
            transport_factory=lambda _device: TorchTransport("cpu"),
            device_class=lambda _device: "test",
        )
        self.service = svc.Flux2KleinInpaintService(self.small, large_models=self.large)

    def held(self) -> dict[str, tuple[int, int]]:
        """`{handle key: (device bytes, host bytes)}` of every FLUX.2 handle (`LargeModelManager.status()`)."""
        return {entry["key"]: (int(entry["device_bytes"]), int(entry["host_bytes"])) for entry in self.large.status()}


def _flat_tables() -> object:
    """A calibrated round-number table with 1 MiB activations (the tiny models need nothing near it)."""
    from modules.ai_backend.runtime.large_models.cost_tables import parse_tables

    def entry(exec_kind: str) -> dict[str, object]:
        return {"family": "*", "phase": "*", "variant": "*", "format": "*", "compute_path": "*", "exec": exec_kind, "measured": True, "fixed_bytes": 1 << 20, "bytes_per_token": 0, "bytes_per_pixel": 0, "reserved_factor": 1.0, "seconds_fixed": 0.0, "seconds_per_ktoken_pass": 0.0, "seconds_per_ktoken2_pass": 0.0, "seconds_per_mpixel": 0.0}

    rate = 1 << 30
    data = {
        "device_class": "test",
        "calibrated": True,
        "source": "test",
        "device": {"runtime_overhead_bytes": 0, "margin_fraction": 0.0, "margin_cap_bytes": 0, "park_bytes_per_s": rate, "restore_bytes_per_s": rate, "block_h2d_bytes_per_s": rate, "disk_cold_bytes_per_s": rate, "disk_warm_bytes_per_s": rate, "prefetch_depth": 1},
        "phases": [entry("device"), entry("cpu")],
    }
    return parse_tables(data, source="test")
