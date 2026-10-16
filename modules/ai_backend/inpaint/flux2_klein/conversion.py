"""
File: modules/ai_backend/inpaint/flux2_klein/conversion.py

Purpose:
The one FLUX.2 rule for transformer checkpoint KEYS: whether a file tensor is
in the BFL (black-forest-labs) layout and, if so, which diffusers state keys
it becomes. The large-model manager's loader applies it tensor by tensor
(`large_model.pipeline_spec`'s `convert`), so the host peak stays one tensor
and nothing here reads a file.

Main responsibilities:
- `is_bfl_key` - the per-key layout decision;
- `convert_bfl_tensor` - diffusers' own per-key converter on ONE tensor;
- `require_known_layout` - the refusal of a checkpoint whose keys carry the
  `model.diffusion_model.` prefix diffusers 0.39's converter does not strip.

Notes:
- The key mapping itself is diffusers' (`convert_flux2_transformer_checkpoint_to_diffusers`)
  and must never be reimplemented here: it is per-key (renames by substring;
  its three special handlers each look at their own key), and its two splits
  (`torch.chunk` of a fused qkv, `swap_scale_shift` of an adaLN weight) are
  WITHIN one tensor, so one key may yield several. Verified on the real klein
  9B header: every BFL key converts on its own into the model's key set.
- Per-key detection is sound for klein because the two layouts share no key:
  BFL names its top-level modules `double_blocks`, `single_blocks`,
  `final_layer`, `img_in`, `txt_in`, `time_in` and its modulations
  `<name>.lin.*`, diffusers `transformer_blocks`, `single_transformer_blocks`,
  `norm_out`, `x_embedder`, `context_embedder`, `time_guidance_embed` and
  `<name>.linear.*`.
- torch / diffusers are imported lazily.
"""

from __future__ import annotations

import logging
from collections.abc import Iterable
from pathlib import Path
from typing import Any

log = logging.getLogger(__name__)

#: The key diffusers 0.39 itself uses to recognize a FLUX.2 checkpoint
#: (`single_file_utils.CHECKPOINT_KEY_NAMES["flux2"]`).
FLUX2_BFL_MARKER_KEY = "single_stream_modulation.lin.weight"

#: Prefix some redistributed checkpoints keep in front of every key; diffusers
#: 0.39's flux2 converter does NOT strip it, so such a file is refused by name.
LDM_UNET_PREFIX = "model.diffusion_model."

_BFL_ONLY_TOPS = frozenset({"double_blocks", "single_blocks", "final_layer", "time_in", "img_in", "txt_in", "guidance_in", "vector_in"})
_BFL_MODULATIONS = frozenset({"double_stream_modulation_img", "double_stream_modulation_txt", "single_stream_modulation"})


def is_bfl_key(file_key: str) -> bool:
    """Whether `file_key` is a BFL-layout transformer key (it must go through `convert_bfl_tensor`)."""
    top, _, rest = file_key.partition(".")
    return top in _BFL_ONLY_TOPS or (top in _BFL_MODULATIONS and rest.startswith("lin."))


def convert_bfl_tensor(name: str, tensor: Any) -> dict[str, Any]:
    """The diffusers `{key: tensor}` one BFL file tensor becomes (three entries for a fused qkv).

    Device-agnostic (it runs on `meta` placeholders while the manager binds a
    component, and on device views when a block streams from disk). The
    converter POPS the keys of the dict it is given, so it gets a fresh one-key
    dict; its `config=` argument is unread in diffusers 0.39.
    """
    from diffusers.loaders.single_file_utils import convert_flux2_transformer_checkpoint_to_diffusers

    return convert_flux2_transformer_checkpoint_to_diffusers({name: tensor}, config={})


def require_known_layout(checkpoint_keys: Iterable[str], *, source: Path) -> None:
    """Refuse a checkpoint whose keys carry `LDM_UNET_PREFIX` (no converter handles it).

    # Raises
    `ValueError` naming the file and the way out.
    """
    if any(key.startswith(LDM_UNET_PREFIX) for key in checkpoint_keys):
        raise ValueError(
            f"Чекпоинт {source.name} хранит веса с приставкой «{LDM_UNET_PREFIX}» в каждом ключе. "
            "Конвертер FLUX.2 в diffusers 0.39 её не убирает, поэтому такой файл не загрузить — "
            "возьмите чекпоинт трансформера без этой приставки."
        )
