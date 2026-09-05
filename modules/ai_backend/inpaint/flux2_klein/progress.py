"""
File: modules/ai_backend/inpaint/flux2_klein/progress.py

Purpose:
The streaming progress protocol of the FLUX.2 klein service: the `phase:"load"`
step numbering every loader and the service itself report against, the callback
type they are handed, and the small adapter that turns a `(step, label)` report
into the four-argument `progress_callback` the IPC layer expects.

Key declarations:
- LOAD_STEP_* / LOAD_PHASE_STEPS - the load-phase step numbers and their total.
- ProgressCb - the `(phase, step, total, label)` callback type.
- _progress_reporter() - binds a phase and a total onto a `ProgressCb`.

Notes:
This module deliberately depends on nothing else in the package: the step
numbers are a wire contract shared by `service.py`, `pipeline.py` and the Rust
client, so they must not drag a loader import behind them.
"""

from __future__ import annotations

from typing import Callable

#: `phase:"load"` progress step numbers, in the order a run actually performs
#: them. The transformer and the VAE are loaded, placed and warmed up FIRST; the
#: text encoder is read only afterwards, when its 16 GB arrive into a host that
#: the transformer has just left (see `inpaint_image_bytes`). The `phase` values
#: on the wire stay `"load"` / `"generate"`, so the Rust side needs no protocol
#: change — only the labels moved.
LOAD_STEP_PREPARE = 0
LOAD_STEP_TRANSFORMER = 1
LOAD_STEP_TOKENIZER = 2
LOAD_STEP_VAE = 3
LOAD_STEP_SCHEDULER = 4
LOAD_STEP_PLACEMENT = 5
LOAD_STEP_WARMUP = 6
LOAD_STEP_TEXT_ENCODER = 7
LOAD_STEP_ENCODE = 8
LOAD_STEP_ENCODER_DONE = 9

#: Total number of `phase:"load"` progress steps, i.e. the last step number above.
LOAD_PHASE_STEPS = LOAD_STEP_ENCODER_DONE

#: Progress callback: (phase, step, total, label). phase in {"load","generate"}.
ProgressCb = Callable[[str, int, int, str], None]


def _progress_reporter(
    callback: ProgressCb | None, phase: str, total: int
) -> Callable[[int, str], None]:
    """Bind a progress callback to one phase; the result never raises."""

    def report(step: int, label: str) -> None:
        if callback is None:
            return
        try:
            callback(phase, int(step), int(total), str(label))
        except Exception:  # noqa: BLE001 - a dead peer must not kill the load
            pass

    return report
