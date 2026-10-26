"""
File: modules/ai_backend/inpaint/flux2_klein/encoder_gguf.py

Purpose:
The name bridge between a llama.cpp GGUF of the Qwen3 text encoder
(`token_embd.weight`, `blk.N.attn_q.weight`, `output_norm.weight`, ...) and the
state keys of the truncated `transformers.Qwen3Model` the encoder spec builds
(`embed_tokens.weight`, `layers.N.self_attn.q_proj.weight`, `norm.weight`).

Main responsibilities:
- `encoder_state_key_of_gguf(name, keep)` - one GGUF tensor name -> its
  `Qwen3Model` state key, `None` for what the truncated model does not load
  (`blk.N` with `N >= keep`, the tied `output.weight` head);
- `is_gguf_encoder_name(name)` - whether a tensor name is in the GGUF name
  space (the two name spaces are disjoint, so the encoder spec dispatches on it);
- `require_known_encoder_names(names, keep, source=...)` - the pre-admission
  refusal of a file with unknown names or missing weights;
- `gguf_encoder_quant_types(spans, keep)` - `{state key: ggml type}` of every
  block-quantized tensor, the input of `runtime.gguf_layers.swap_gguf_modules`.

Notes:
- There is NO hand-written name table: the mapping is the `gguf` package's own
  QWEN3 tensor-name map (`gguf.get_tensor_name_map`), used inverted over the
  REAL `Qwen3Model` key set (a tiny template model built on `meta`). The map
  alone is not invertible (several HF spellings map to one GGUF name), the key
  set of the class we load is what picks the one spelling.
- No Q/K row permutation: llama.cpp permutes Q/K rows only for the `llama`
  architecture, never for `qwen3` (verified on real Qwen3-4B Q8_0 rows against
  the bf16 checkpoint: identity rel. error 0.0056, permuted 1.41), so a GGUF
  tensor is the HF tensor under another name.
- `gguf`, torch, transformers and accelerate are imported lazily (the key set
  is built once per process and cached).
"""

from __future__ import annotations

import functools
import logging
import re
from collections.abc import Iterable, Sequence
from pathlib import Path
from typing import Any

log = logging.getLogger(__name__)

#: The prefix of the HF causal-LM checkpoint names the `gguf` name map is
#: written for (`model.layers.0.self_attn.q_proj`); `Qwen3Model`'s own state
#: keys have no prefix.
_HF_MODEL_PREFIX = "model."

#: Suffixes `gguf.TensorNameMap.get_name` may carry over from an HF key.
_TRY_SUFFIXES = (".weight", ".bias")

_GGUF_BLOCK_RE = re.compile(r"^blk\.(\d+)\.(.+)$")
_LAYER_PREFIX_0 = "layers.0."

#: How many names a refusal lists before it says "and N more".
_NAMES_IN_MESSAGE = 5


@functools.lru_cache(maxsize=1)
def _qwen3_name_tables() -> tuple[dict[str, str], frozenset[str], str]:
    """`({canonical GGUF name: Qwen3Model state key}, GGUF root names, the lm-head GGUF name)`.

    Canonical = block 0 (`blk.0.*` <-> `layers.0.*`); other blocks are
    substituted by index. Built from a one-layer template `Qwen3Model` on
    `meta` (its persistent state keys) mapped through the `gguf` package's
    QWEN3 map.

    # Raises
    `RuntimeError` when the installed `gguf` package cannot name a key of the
    installed `Qwen3Model` (incompatible package versions).
    """
    import gguf
    from accelerate import init_empty_weights
    from transformers import Qwen3Config, Qwen3Model

    # The template's sizes are irrelevant: only the structure (which modules
    # exist) decides the key set. `attention_bias` stays the Qwen3 default
    # (False), as in every Qwen3 checkpoint FLUX.2 klein ships.
    config = Qwen3Config(vocab_size=32, hidden_size=32, intermediate_size=32, num_hidden_layers=1, num_attention_heads=2, num_key_value_heads=1, head_dim=16)
    with init_empty_weights():
        template = Qwen3Model(config)
    name_map = gguf.get_tensor_name_map(gguf.MODEL_ARCH.QWEN3, 1)
    inverse: dict[str, str] = {}
    for state_key in template.state_dict():
        gguf_name = name_map.get_name(f"{_HF_MODEL_PREFIX}{state_key}", try_suffixes=_TRY_SUFFIXES)
        if gguf_name is None:
            raise RuntimeError(f"Пакет gguf не знает имени тензора Qwen3 «{_HF_MODEL_PREFIX}{state_key}»: версии gguf и transformers несовместимы.")
        inverse[gguf_name] = state_key
    roots = frozenset(name.split(".", 1)[0] for _, name in name_map.mapping.values())
    head = f"{gguf.TENSOR_NAMES[gguf.MODEL_TENSOR.OUTPUT]}.weight"
    return inverse, roots, head


def is_gguf_encoder_name(name: str) -> bool:
    """Whether `name` is in the llama.cpp GGUF name space (`blk.*`, `token_embd.*`, `output*`, ...).

    The HF names of a safetensors encoder (`model.*`, `lm_head.*`) never are,
    so one encoder spec can serve both containers.
    """
    _, roots, _ = _qwen3_name_tables()
    return name.split(".", 1)[0] in roots


def encoder_state_key_of_gguf(name: str, keep: int) -> str | None:
    """The truncated `Qwen3Model` state key of the GGUF tensor `name`.

    `token_embd.weight` -> `embed_tokens.weight`, `output_norm.weight` ->
    `norm.weight`, `blk.N.<x>` -> `layers.N.<y>`. `None` for `blk.N` with
    `N >= keep` (the truncation: such a tensor is never read) and for the tied
    `output.weight` head (`Qwen3Model` has none).

    # Raises
    `ValueError` for any other name the QWEN3 map does not lead to a key of
    `Qwen3Model` (a bias the model lacks, `rope_freqs`, a foreign tensor).
    """
    inverse, _, head = _qwen3_name_tables()
    if name == head:
        return None
    index: int | None = None
    canonical = name
    match = _GGUF_BLOCK_RE.match(name)
    if match is not None:
        index = int(match.group(1))
        if index >= keep:
            return None
        canonical = f"blk.0.{match.group(2)}"
    state_key = inverse.get(canonical)
    if state_key is None:
        raise ValueError(f"тензор «{name}» не соответствует ни одному весу Qwen3Model")
    if index is None:
        return state_key
    if not state_key.startswith(_LAYER_PREFIX_0):
        raise ValueError(f"тензор «{name}» блока {index} соответствует весу вне слоёв ({state_key})")
    return f"layers.{index}.{state_key[len(_LAYER_PREFIX_0):]}"


def _expected_state_keys(keep: int) -> set[str]:
    """Every state key of a `Qwen3Model` truncated to `keep` layers."""
    inverse, _, _ = _qwen3_name_tables()
    expected: set[str] = set()
    for state_key in inverse.values():
        if state_key.startswith(_LAYER_PREFIX_0):
            expected.update(f"layers.{index}.{state_key[len(_LAYER_PREFIX_0):]}" for index in range(keep))
        else:
            expected.add(state_key)
    return expected


def _listed(names: Sequence[str]) -> str:
    shown = ", ".join(names[:_NAMES_IN_MESSAGE])
    rest = len(names) - _NAMES_IN_MESSAGE
    return f"{shown} и ещё {rest}" if rest > 0 else shown


def require_known_encoder_names(names: Iterable[str], keep: int, *, source: Path) -> None:
    """Refuse a GGUF encoder file before any tensor is read: every name maps (or is skipped) and every kept weight is present.

    `names` are the file's tensor names, `keep` the decoder layers the model
    keeps (`components.ENCODER_KEEP_LAYERS`).

    # Raises
    `ValueError` naming `source` and the unknown tensors, or the state keys
    of the truncated model no tensor fills (an incomplete or foreign file).
    """
    unknown: list[str] = []
    produced: set[str] = set()
    for name in names:
        try:
            state_key = encoder_state_key_of_gguf(name, keep)
        except ValueError:
            unknown.append(name)
            continue
        if state_key is not None:
            produced.add(state_key)
    if unknown:
        log.error("FLUX.2 klein: GGUF текстового энкодера %s содержит тензоры вне Qwen3: %s", source, unknown)
        raise ValueError(
            f"Текстовый энкодер {source.name}: тензоры {_listed(unknown)} не относятся к модели Qwen3 — "
            "это не GGUF текстового энкодера FLUX.2 klein (нужен Qwen3 в формате llama.cpp)."
        )
    missing = sorted(_expected_state_keys(keep) - produced)
    if missing:
        log.error("FLUX.2 klein: в GGUF текстового энкодера %s нет весов %s", source, missing)
        raise ValueError(
            f"Текстовый энкодер {source.name}: в файле нет весов {_listed(missing)} — файл неполный "
            "или это не Qwen3. Скачайте файл энкодера заново."
        )


def gguf_encoder_quant_types(spans: Sequence[Any], keep: int) -> dict[str, int]:
    """`{Qwen3Model state key: ggml type}` of every block-quantized tensor the truncated model loads.

    `spans` are the file's `TensorSpan`s; plain (F32 / F16 / BF16) tensors and
    skipped names are absent.

    # Raises
    `ValueError` for an unknown name (`encoder_state_key_of_gguf`).
    """
    found: dict[str, int] = {}
    for span in spans:
        if span.quant_type is None:
            continue
        state_key = encoder_state_key_of_gguf(span.name, keep)
        if state_key is not None:
            found[state_key] = int(span.quant_type)
    return found

