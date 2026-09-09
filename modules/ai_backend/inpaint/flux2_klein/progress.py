"""
File: modules/ai_backend/inpaint/flux2_klein/progress.py

Purpose:
The streaming progress protocol of the FLUX.2 klein service: the `phase:"load"`
step numbering every loader and the service itself report against, the callback
type they are handed, and the small adapters that turn a `(step, label)` report —
or a byte count inside one such step — into the `progress_callback` the IPC layer
expects.

Key declarations:
- LOAD_STEP_* / LOAD_PHASE_STEPS - the load-phase step numbers and their total.
- ProgressCb - the `(phase, step, total, label)` callback type, plus the three
  OPTIONAL keyword-only fields that carry the second, byte level.
- FileProgressCb - the `(done_bytes, total_bytes)` callback a loader is handed.
- _progress_reporter() - binds a phase and a total onto a `ProgressCb`.
- _file_progress_reporter() - binds ONE step frame so a loader can report bytes
  inside it without moving `step` / `total`.

Notes:
This module deliberately depends on nothing else in the package: the step
numbers are a wire contract shared by `service.py`, `pipeline.py` and the Rust
client, so they must not drag a loader import behind them.
"""

from __future__ import annotations

from typing import Callable, Protocol

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

class ProgressCb(Protocol):
    """Progress callback: `(phase, step, total, label)`, phase in {"load","generate"}.

    The three keyword-only fields are the SECOND, optional level of the same
    frame — the byte counters of the file currently being read
    (`file_step`/`file_total`/`file_label` on the wire). They are additive:
    `step`/`total` keep meaning the overall step level, a field left `None` is
    omitted from the frame entirely, and a caller that passes only the four
    positional arguments emits exactly the frame this protocol always described.
    The Rust client draws the second level as its own bar for ANY frame carrying
    it and clears it again on the next frame that does not.

    An implementation may accept the four positional arguments only: nothing in
    this package requires the second level, and both reporters below swallow the
    `TypeError` such a callback raises (see `_file_progress_reporter`).
    """

    def __call__(
        self,
        phase: str,
        step: int,
        total: int,
        label: str,
        *,
        file_step: int | None = None,
        file_total: int | None = None,
        file_label: str | None = None,
    ) -> None: ...


#: Byte-level progress callback handed to a component loader: `(done_bytes,
#: total_bytes)`. It carries no name — the file it describes is bound when the
#: reporter is built, because the file does not change during one load.
FileProgressCb = Callable[[int, int], None]


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


def _file_progress_reporter(
    callback: ProgressCb | None,
    phase: str,
    total: int,
    step: int,
    label: str,
    file_label: str,
) -> FileProgressCb:
    """Bind ONE step frame so a loader can report BYTES inside it.

    The returned `report(done_bytes, total_bytes)` re-emits the very frame the
    step-level reporter emitted — same `phase`, `step`, `total` and `label` — and
    only ADDS the three optional file fields. That is what keeps the overall bar
    parked on this step while the second bar fills: the byte level must never
    move `step`/`total`, which are a wire contract shared with the Rust client
    (`LOAD_STEP_*`).

    `file_label` names the FILE being read (a checkpoint name), not the tensor
    inside it: the client renders it as «<label> — <done> из <total> ГиБ», and a
    tensor key would flicker on every frame while saying nothing the user can
    act on.

    Like `_progress_reporter`, the result never raises: a dead IPC peer, or a
    callback that only accepts the four positional arguments, must not kill a
    load that is otherwise succeeding.
    """

    def report(done_bytes: int, total_bytes: int) -> None:
        if callback is None:
            return
        try:
            callback(
                phase,
                int(step),
                int(total),
                str(label),
                file_step=int(done_bytes),
                file_total=int(total_bytes),
                file_label=str(file_label),
            )
        except Exception:  # noqa: BLE001 - a dead or older peer must not kill the load
            pass

    return report
