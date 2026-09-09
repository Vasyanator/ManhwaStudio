"""
File: modules/ai_backend/runtime/error_text.py

Purpose:
Sanitize Torch/ROCm runtime error text before it is shown to the user, so the
product never republishes an upstream suggestion that is actively harmful on
this project's ROCm build.

Main responsibilities:
- strip PyTorch's out-of-memory advice to set
  `PYTORCH_CUDA_ALLOC_CONF` / `PYTORCH_HIP_ALLOC_CONF` to
  `expandable_segments:True`, and put a short honest note in its place;
- do so ONLY on a ROCm runtime, where that advice is harmful, and pass the
  message through untouched everywhere else;
- leave every other byte of the message untouched, because the rest of the
  Torch text is the diagnostic the user may have to forward.

Key functions:
- `configure_error_text()`
- `rocm_advice_rewrite_enabled()`
- `sanitize_torch_error()`

Notes:
- Deliberately dependency-free: standard library only, no `torch`, no sibling
  backend module. It is imported by the torch-free `ipc/` layer, which must
  not gain an AI-stack import through it (see `runtime/MODULE_README.md`).
  That is also why the ROCm fact is PUSHED in by
  `rocm_runtime.configure_rocm_runtime()` instead of being probed here.
- Why the advice is removed at all: on the ROCm/HIP runtime this project
  targets (torch 2.12.0+rocm7.2, HIP 7.2, gfx1201) `expandable_segments:True`
  corrupts computation - VAE decode above 256 px produces NaN in a different
  module on every run, and at 512-1536 px the process takes
  `HSA_STATUS_ERROR_ILLEGAL_INSTRUCTION` and wedges the HSA queue. The
  user-visible symptom is a completely black image with normal timings, so a
  user who follows Torch's advice gets a silent wrong result, not an error.
  `rocm_runtime.configure_rocm_runtime()` forces the setting to `False`.
- Why it is removed only there: on NVIDIA/CUDA the same advice is CORRECT and
  useful, and the replacement note asserts a ROCm-specific claim that is simply
  false on that hardware. The default is therefore "not ROCm": a process that
  never called `configure_error_text` (unit tests, a CPU or ONNX-only install)
  passes Torch's text through byte-identical.
- On this HIP build Torch still emits the CUDA spelling of the variable
  (verified in `libc10_hip.so`), so both spellings are handled.
- The transformation is idempotent and never raises: it runs on an error path,
  where a second failure would replace a real diagnostic with nothing.
"""

from __future__ import annotations

import logging
import re
import threading

log = logging.getLogger(__name__)

_STATE_LOCK = threading.Lock()
_ROCM_RUNTIME = False

# The advice clause as Torch emits it, e.g.
#   " If reserved but unallocated memory is large try setting
#     PYTORCH_CUDA_ALLOC_CONF=expandable_segments:True to avoid fragmentation."
# The match is keyed on the ENV-VAR ASSIGNMENT, not on the bare
# `expandable_segments:True` token: Torch uses that token in unrelated messages
# (the CUDA-IPC / pidfd warnings) that must stay intact.
# What precedes the assignment is ANCHORED on Torch's own advice wording
# (`try setting`, optionally preceded by its `If ...` condition) rather than on
# "arbitrary sentence text". An unanchored `[^.\n]*?` prefix looks harmless
# because `[^.\n]` cannot cross a sentence boundary - but a Torch OOM message
# whose numbers carry no period ("tried to allocate 10 GiB; GPU has 2 GiB free;
# try setting ...") has no boundary to stop at, and the whole diagnostic is
# eaten. The numbers are the part the user has to forward.
# `[^.\n]` after the assignment still keeps the tail inside one sentence and one
# line. The leading run of spaces is captured and re-emitted so the preceding
# sentence keeps its original spacing.
_EXPANDABLE_SEGMENTS_ADVICE_RE = re.compile(
    r"(?P<lead>[ \t]*)"
    r"(?:\bif\b[^.\n]*?)?"
    r"\btry\s+setting\s+"
    r"PYTORCH_(?:CUDA|HIP)_ALLOC_CONF\s*=\s*expandable_segments\s*:\s*True"
    r"[^.\n]*\.?",
    re.IGNORECASE,
)

# Replacement for the removed clause. It must NOT itself contain
# `PYTORCH_*_ALLOC_CONF=expandable_segments:True`, or a second pass would match
# and mangle it - that absence is what makes `sanitize_torch_error` idempotent.
_EXPANDABLE_SEGMENTS_NOTE = (
    "ManhwaStudio: не включайте expandable_segments — на ROCm-сборке этого "
    "проекта такой режим аллокатора портит вычисления и даёт полностью чёрное "
    "изображение; бэкенд принудительно держит его выключенным."
)


def configure_error_text(*, rocm_runtime: bool) -> None:
    """Record whether this process runs on a ROCm/HIP Torch build.

    Called once at startup by `rocm_runtime.configure_rocm_runtime()`, which is
    the only place that may import torch to find out. Until it is called the
    answer is `False`, so a process that never configures anything (unit tests, a
    CPU or ONNX-only install) leaves Torch's text alone. Safe to call repeatedly
    and from any thread.
    """
    global _ROCM_RUNTIME
    with _STATE_LOCK:
        _ROCM_RUNTIME = bool(rocm_runtime)


def rocm_advice_rewrite_enabled() -> bool:
    """Whether `sanitize_torch_error` currently rewrites anything.

    `True` only after `configure_error_text(rocm_runtime=True)`. Exposed so a
    caller (and the tests) can tell "nothing matched" from "the rewrite is off".
    """
    with _STATE_LOCK:
        return _ROCM_RUNTIME


def sanitize_torch_error(text: str) -> str:
    """Return `text` with Torch's harmful `expandable_segments:True` advice replaced.

    `text` is an error message on its way to the user (typically `str(exc)` of a
    handler failure). On a ROCm runtime every occurrence of Torch's "try setting
    `PYTORCH_{CUDA,HIP}_ALLOC_CONF=expandable_segments:True`" clause is replaced
    by a short note explaining that the setting must not be enabled on this
    project's ROCm build; everything else is returned byte-identical.

    OFF this project's ROCm build the text is returned byte-identical in full:
    Torch's advice is correct on NVIDIA/CUDA, and the replacement note makes a
    ROCm-specific claim that would be false there. The switch is
    `configure_error_text`, and its default is "not ROCm".

    The function is idempotent (its own output contains no matchable clause) and
    never raises: a non-string argument is coerced with `str()`, and any internal
    failure returns the input unchanged rather than losing the diagnostic. The
    UNTOUCHED text must already have been written to the process log by the
    caller - this function is for the user-facing copy only.
    """
    try:
        if not isinstance(text, str):
            text = str(text)
        if not text:
            return text
        if not rocm_advice_rewrite_enabled():
            return text
        return _EXPANDABLE_SEGMENTS_ADVICE_RE.sub(
            lambda match: f"{match.group('lead')}{_EXPANDABLE_SEGMENTS_NOTE}", text
        )
    except Exception as exc:  # noqa: BLE001 - error path: never lose the diagnostic
        # Best-effort by contract: on an error path, returning the original text
        # is strictly better than propagating a second failure. It is still
        # LOGGED, at debug - a swallowed failure with no trace at all is how a
        # sanitizer that silently stopped working stays unnoticed.
        log.debug("sanitize_torch_error failed, returning the text unchanged: %s", exc)
        try:
            return text if isinstance(text, str) else str(text)
        except Exception as coercion_exc:  # noqa: BLE001 - a __str__ that raises
            log.debug("sanitize_torch_error could not coerce its argument: %s", coercion_exc)
            return ""
