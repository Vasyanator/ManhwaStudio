"""
File: modules/ai_backend/inpaint/flux3_klein/phases.py

Purpose:
Named phases of one FLUX.3 klein generation and the observer hook a caller (the
VRAM measurement script, later the service's progress) attaches to them.

Key declarations:
- `Phase` - the phases, in run order
- `PhaseObserver` - `(phase, "begin" | "end", info) -> None`
- `observed()` - context manager that brackets one phase

Notes:
The observer is an in-process caller, not an IPC peer, so its exceptions
PROPAGATE (unlike `ByteProgress`, which swallows a dead peer's error): a
measurement that silently lost a phase would report wrong numbers.
"""

from __future__ import annotations

import logging
from collections.abc import Callable, Iterator, Mapping
from contextlib import contextmanager
from enum import Enum
from typing import Any, Literal

log = logging.getLogger(__name__)


class Phase(Enum):
    """One step of a generation, in run order (DiT/AE load first: only the leased
    load may run in the load scope).

    The three DiT moves (`DIT_OFFLOAD` after the load scope, `DIT_RESTORE`
    before the denoise, `DIT_OFFLOAD_DECODE` before the decode) are separate
    phases so their cost is measurable; each is reported on every call, as a
    no-op when the DiT does not move.
    """

    DIT_LOAD = "dit_load"
    AE_LOAD = "ae_load"
    DIT_OFFLOAD = "dit_offload"
    ENCODER_LOAD = "encoder_load"
    ENCODE = "encode"
    ENCODER_RELEASE = "encoder_release"
    REF_ENCODE = "ref_encode"
    DIT_RESTORE = "dit_restore"
    DENOISE = "denoise"
    DIT_OFFLOAD_DECODE = "dit_offload_decode"
    DECODE = "decode"


#: `observer(phase, event, info)`. `info` holds the keyword arguments given to
#: `observed`; the `"end"` call adds `failed: bool`.
PhaseObserver = Callable[[Phase, Literal["begin", "end"], Mapping[str, Any]], None]


@contextmanager
def observed(observer: PhaseObserver | None, phase: Phase, **info: Any) -> Iterator[None]:
    """Bracket the body as `phase`: `observer(phase, "begin", info)` before it and
    `observer(phase, "end", {**info, "failed": <body raised>})` after it.

    `None` observes nothing. The `"end"` call happens whether the body succeeded
    or raised; the body's exception then propagates. An exception raised BY the
    observer propagates too: from `"begin"` the body does not run and no `"end"`
    follows; from `"end"` it replaces a body exception (which stays chained as
    its `__context__`).
    """
    if observer is None:
        yield
        return
    observer(phase, "begin", dict(info))
    failed = True
    try:
        yield
        failed = False
    finally:
        observer(phase, "end", {**info, "failed": failed})
