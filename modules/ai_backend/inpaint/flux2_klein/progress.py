"""
File: modules/ai_backend/inpaint/flux2_klein/progress.py

Purpose:
The streaming progress protocol of the FLUX.2 klein service: the `phase:"load"`
step numbering every loader and the service itself report against, the callback
type they are handed, and the small adapters that turn a `(step, label)` report —
or the large-model manager's disk-read frames — into the `progress_callback`
the IPC layer expects.

Key declarations:
- LOAD_STEP_* / LOAD_PHASE_STEPS - the load-phase step numbers and their total.
- ProgressCb - the `(phase, step, total, label)` callback type, plus the four
  OPTIONAL keyword-only fields that carry the second, byte level.
- FILE_COMPONENT_* - the `file_component` values naming the component read.
- _progress_reporter() - binds a phase and a total onto a `ProgressCb`.
- LoadFrames - the request's `ProgressCb` plus the manager's load-progress
  sink: re-emits the current step frame with the byte level of a component
  read, without moving `step` / `total`.

Notes:
This module deliberately depends on nothing else in the package: the step
numbers are a wire contract shared by `service.py`, `pipeline.py` and the Rust
client, so they must not drag a loader import behind them.
"""

from __future__ import annotations

from collections.abc import Mapping
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

#: `file_component` values of a load-phase byte level (v21): which component is
#: being read. A wire contract with the Rust client, which reads an absent field
#: as the transformer (frames of older backends).
FILE_COMPONENT_TRANSFORMER = "transformer"
FILE_COMPONENT_TEXT_ENCODER = "text_encoder"


class ProgressCb(Protocol):
    """Progress callback: `(phase, step, total, label)`, phase in {"load","generate"}.

    The keyword-only fields are the SECOND, optional level of the same
    frame — the byte counters of the file currently being read
    (`file_step`/`file_total`/`file_label` on the wire) and the component it
    belongs to (`file_component`, `FILE_COMPONENT_*`). They are additive:
    `step`/`total` keep meaning the overall step level, a field left `None` is
    omitted from the frame entirely, and a caller that passes only the four
    positional arguments emits exactly the frame this protocol always described.
    The Rust client draws the second level as its own bar for ANY frame carrying
    it and clears it again on the next frame that does not.

    An implementation may accept the four positional arguments only: nothing in
    this package requires the second level, and `_progress_reporter` and
    `LoadFrames.sink` swallow the `TypeError` such a callback raises.
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
        file_component: str | None = None,
    ) -> None: ...


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


class ReadEvent(Protocol):
    """The fields of the manager's `LoadProgress` frame this module reads (`runtime.large_models.loader`)."""

    @property
    def component(self) -> str: ...

    @property
    def done_bytes(self) -> int: ...

    @property
    def total_bytes(self) -> int: ...


class LoadFrames:
    """A request's `ProgressCb` that also turns the manager's disk-read frames into the wire's SECOND (byte) level.

    Every step-level frame (a call without file fields: a `load` step or a
    `generate` step) passes straight through and is remembered. `sink` is the
    `Session.set_load_progress` sink of every session of the request: a read
    of a component named in `files` re-emits the LAST step-level frame — same
    `phase`, `step`, `total`, `label` — with only `file_step` / `file_total`
    (bytes of this read) / `file_label` / `file_component` added. So the byte
    level never moves `step` / `total` (`LOAD_STEP_*`, a contract with the
    Rust client): a read during the load phase rides on the load step that
    announced it (`LOAD_STEP_TRANSFORMER`, `LOAD_STEP_TEXT_ENCODER`), and a
    re-read inside a phase (a DISK-tier component brought back, a degrade
    retry's reload) rides on the current step of that phase, a `generate`
    frame during the denoise.

    `files`: `{manager component name: (file_component, file_label)}`;
    `file_label` names the file (single file, GGUF) or the folder of a
    sharded component — never a tensor key, which would flicker on every frame.
    Components absent from it (the VAE: tens of MB) send no byte frames.

    `__call__` propagates what the wrapped callback raises (a raising
    `generate` callback aborts the run, as it always did); `sink` never raises,
    so a dead peer cannot kill a read that is otherwise succeeding.
    """

    def __init__(self, callback: ProgressCb, files: Mapping[str, tuple[str, str]]) -> None:
        self._callback = callback
        self._files = dict(files)
        self._last: tuple[str, int, int, str] | None = None

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
        file_component: str | None = None,
    ) -> None:
        file_level = {
            key: value
            for key, value in (("file_step", file_step), ("file_total", file_total), ("file_label", file_label), ("file_component", file_component))
            if value is not None
        }
        if not file_level:
            self._last = (phase, int(step), int(total), str(label))
        self._callback(phase, step, total, label, **file_level)

    def sink(self, event: ReadEvent) -> None:
        """`Session.set_load_progress` sink: one byte frame on the current step.

        Nothing is sent before the first step frame, for a component absent
        from `files`, or for the empty frame of a read that reads nothing
        (`total_bytes == 0`: a bar over zero bytes says nothing).
        """
        named = self._files.get(event.component)
        last = self._last
        if named is None or last is None or event.total_bytes <= 0:
            return
        file_component, file_label = named
        phase, step, total, label = last
        try:
            self._callback(
                phase,
                step,
                total,
                label,
                file_step=int(event.done_bytes),
                file_total=int(event.total_bytes),
                file_label=file_label,
                file_component=file_component,
            )
        except Exception:  # noqa: BLE001 - a dead or older peer must not kill the read
            pass
