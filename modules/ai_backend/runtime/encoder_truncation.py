"""
File: modules/ai_backend/runtime/encoder_truncation.py

Purpose:
The one rule for how many decoder layers a text encoder must keep when a
pipeline reads only some of its `output_hidden_states`, so that everything
above the last requested state can be left unloaded.

Main responsibilities:
- `keep_layers_for_hidden_states` - `max(indices) + 1`, with the reason the
  `+ 1` is exact.

Notes:
- Stdlib only. How a service applies the count (config kwargs, which weight
  prefix it skips) is model-specific and stays in that service's package.
"""

from __future__ import annotations

import logging
from collections.abc import Sequence

log = logging.getLogger(__name__)


def keep_layers_for_hidden_states(indices: Sequence[int]) -> int:
    """Decoder layers a text encoder must keep so `hidden_states[i]` stays raw for every `i`.

    `indices` are the `output.hidden_states` indices the pipeline reads
    (`hidden_states[0]` is the embedding output, `hidden_states[n]` the output
    of decoder layer `n`). Everything above the returned count is dead weight:
    those layers are computed, stored and thrown away.

    **The `+ 1` is exact and must not be shaved.** `hidden_states[n]` for
    `n == num_hidden_layers` is not a raw layer output but the output of
    `model.norm` applied to the last layer — measured on a random Qwen3, a
    6-layer and a 4-layer model agree bit-for-bit on `hidden_states[1..3]` and
    differ by 2.53 on `hidden_states[4]`. Keeping `max(indices)` layers instead
    of `max(indices) + 1` would therefore silently feed the consumer a normed
    tensor where a raw one belongs. Keeping MORE than this changes nothing but
    the cost: the states at the requested indices are bit-identical to those of
    the truncated model, which is why embeddings computed before a truncation
    stay valid.

    # Raises
    `ValueError` when `indices` is empty or holds a negative index (a negative
    index counts from the END and therefore cannot be served by truncation).
    """
    if len(indices) == 0:
        raise ValueError("Не заданы индексы скрытых состояний текстового энкодера")
    negative = [index for index in indices if index < 0]
    if negative:
        raise ValueError(
            f"Индексы скрытых состояний текстового энкодера должны быть неотрицательными: {negative}"
        )
    return max(indices) + 1
