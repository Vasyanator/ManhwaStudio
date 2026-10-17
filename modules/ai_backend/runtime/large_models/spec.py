"""
File: modules/ai_backend/runtime/large_models/spec.py

Purpose:
The declaration a model package hands the large-model core: which components
the model has, how a checkpoint tensor maps onto a residency block, which
phases a run goes through and how much work each phase does for a request.

Main responsibilities:
- the request description (`RequestShape`) and one phase's work for it
  (`PhaseWork`);
- the per-component declaration (`ComponentSpec`, `Role`, `ExecDevice`);
- the per-phase declaration (`PhaseSpec`) and the whole model (`LargeModelSpec`,
  validated on construction);
- one weight format's facts (`FormatInfo`);
- a model-specific degrade rung (`Fallback`).

Notes:
- Stdlib only, the bottom of the package: every other module may import it.
- The core never names a model family; `LargeModelSpec.family` is only a key into
  the calibration tables.
- Loader-side fields (`ComponentSpec.build_meta`, `ComponentSpec.convert`)
  are consumed only by the mechanics layer (`loader`, `hooks`); sizing,
  planning and forecasting never call them.
- Manager-side fields (`LargeModelSpec.formats`, `key_of`, `fallbacks`) are
  consumed only by `manager` / `degrade`; `host_pin_of` by the planner (where
  the pinned components are placed) and by `key` (the residency key).
"""

from __future__ import annotations

import re
from collections.abc import Callable, Collection, Iterable, Mapping, Sequence
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Any


class Role(Enum):
    """What a component is to a profile template; templates are written per role."""

    DIT = "dit"
    ENCODER = "encoder"
    AE = "ae"


class ExecDevice(Enum):
    """Where a component's forward runs: on the run device, or on the host CPU."""

    RUN_DEVICE = "run_device"
    CPU = "cpu"


@dataclass(frozen=True)
class RequestShape:
    """The request facts a forecast depends on, in model-agnostic terms.

    `width`/`height` are the generated region in pixels; `references` the
    reference images as `(width, height)` pairs in pixels, BEFORE any model-side
    resize (the spec applies its own limits); `prompt_tokens` and `steps` are
    `None` when the caller does not know them and the spec's defaults apply.
    """

    width: int
    height: int
    references: tuple[tuple[int, int], ...] = ()
    prompt_tokens: int | None = None
    steps: int | None = None

    def __post_init__(self) -> None:
        if self.width <= 0 or self.height <= 0:
            raise ValueError(f"RequestShape: размер области должен быть положительным, получено {self.width}x{self.height}")
        for ref_width, ref_height in self.references:
            if ref_width <= 0 or ref_height <= 0:
                raise ValueError(f"RequestShape: некорректный размер референса {ref_width}x{ref_height}")
        if self.prompt_tokens is not None and self.prompt_tokens < 0:
            raise ValueError(f"RequestShape: prompt_tokens < 0 ({self.prompt_tokens})")
        if self.steps is not None and self.steps <= 0:
            raise ValueError(f"RequestShape: steps должно быть > 0 ({self.steps})")


@dataclass(frozen=True)
class PhaseWork:
    """How much work one phase does for one request, as the cost tables measure it.

    `tokens` is the sequence length the phase's compute component sees (joint
    image + reference + text tokens for a DiT denoise, text tokens for an
    encode); `pixels` the pixel count the phase processes (decoded image, the
    LARGEST reference for a reference encode); `passes` how many forward passes
    of the compute component the phase runs (denoise steps; 1 elsewhere) — it
    multiplies the per-pass time and every per-pass block copy; `variant` selects
    a calibration entry when one phase has several cost profiles (e.g.
    `"float32"` vs `"bfloat16"` autoencoder, `"text"` vs `"vl"` encoder);
    `extra_device_bytes` is a model-specific device term the tables cannot
    express (e.g. an attention mask), added after the reserved factor.
    """

    tokens: int = 0
    pixels: int = 0
    passes: int = 1
    variant: str = ""
    extra_device_bytes: int = 0

    def __post_init__(self) -> None:
        if self.tokens < 0 or self.pixels < 0 or self.extra_device_bytes < 0:
            raise ValueError(f"PhaseWork: отрицательный объём работы {self!r}")
        if self.passes < 1:
            raise ValueError(f"PhaseWork: passes должно быть >= 1 ({self.passes})")


def _identity_state_keys(file_key: str) -> Sequence[str]:
    """Default `ComponentSpec.state_keys_of`: a file tensor is one state entry of the same name."""
    return (file_key,)


def _keep_file_itemsize(file_key: str, dtype_token: str) -> int | None:
    """Default `ComponentSpec.device_itemsize`: the tensor keeps its file dtype on the device."""
    return None


_NATURAL_SPLIT = re.compile(r"(\d+)")


def natural_block_order(names: Sequence[str]) -> list[str]:
    """Sort block names so `blocks.10` follows `blocks.9` (digit runs compare as numbers)."""

    def _key(name: str) -> tuple[tuple[int, int | str], ...]:
        return tuple((0, int(part)) if part.isdigit() else (1, part) for part in _NATURAL_SPLIT.split(name) if part)

    return sorted(names, key=_key)


@dataclass(frozen=True)
class ComponentSpec:
    """One component of a large model (DiT, text encoder, autoencoder).

    `files(options, format)` returns the checkpoint files of this component for
    one weight format; `block_of(state_key)` names the residency block a state
    entry belongs to, `None` for weights that always travel with the component
    (embedders, final layers, a monolithic autoencoder); `state_keys_of(file_key)`
    lists the state entries one file tensor becomes (several for a 1:N
    conversion, none for a tensor the model does not load); `device_itemsize(
    file_key, dtype_token)` the bytes per element on the device, `None` = the
    file's own; `exec_devices_allowed` where the component may run its forward;
    `block_order` orders block names (the first `k` of that order are the ones a
    plan keeps resident), natural sort when `None`.

    Loader fields (the mechanics layer only):
    - `build_meta(options, format)` builds the component's `nn.Module` on the
      `meta` device (non-persistent buffers already real); `None` = this spec
      cannot be loaded by `loader.load_component`.
    - `convert(file_key, tensor)` yields the `(state_key, tensor)` pairs one
      file tensor becomes and must yield exactly `state_keys_of(file_key)`;
      `None` = identity, valid only when every file tensor maps to ONE state
      key. It must be device-agnostic torch code: the DISK tier applies it to
      device views of the file bytes on every pass.
    - A block name must be the dotted path of the submodule that owns exactly
      that block's state keys (the residency hooks attach to it).
    """

    name: str
    role: Role
    files: Callable[[Any, str], Sequence[Path]]
    block_of: Callable[[str], str | None]
    state_keys_of: Callable[[str], Sequence[str]] = _identity_state_keys
    device_itemsize: Callable[[str, str], int | None] = _keep_file_itemsize
    exec_devices_allowed: frozenset[ExecDevice] = frozenset({ExecDevice.RUN_DEVICE})
    block_order: Callable[[Sequence[str]], Sequence[str]] | None = None
    build_meta: Callable[[Any, str], Any] | None = None
    convert: Callable[[str, Any], Iterable[tuple[str, Any]]] | None = None

    def __post_init__(self) -> None:
        if not self.name:
            raise ValueError("ComponentSpec: пустое имя компонента")
        if not self.exec_devices_allowed:
            raise ValueError(f"ComponentSpec «{self.name}»: не разрешено ни одного устройства исполнения")

    def ordered_blocks(self, names: Sequence[str]) -> list[str]:
        """`names` in this component's block order (see `block_order`)."""
        if self.block_order is None:
            return natural_block_order(names)
        ordered = list(self.block_order(names))
        if sorted(ordered) != sorted(names):
            raise ValueError(f"ComponentSpec «{self.name}»: block_order вернул другой набор блоков")
        return ordered


@dataclass(frozen=True)
class PhaseSpec:
    """One phase of a run (encode, denoise, decode, …), in run order.

    `uses` are the components whose weights the phase needs; `compute` the one
    whose forward the phase runs (it decides the activation's device and the
    calibration entry; must be in `uses`); `work(shape, options)` the phase's
    work for one request.
    """

    name: str
    uses: frozenset[str]
    compute: str
    work: Callable[[RequestShape, Any], PhaseWork]

    def __post_init__(self) -> None:
        if self.compute not in self.uses:
            raise ValueError(f"PhaseSpec «{self.name}»: compute «{self.compute}» не входит в uses")


#: Phase names the core reserves for its own pseudo-phases (the between-runs
#: state and the transitions); a model phase may not use them.
RESERVED_PHASE_NAMES = frozenset({"idle"})


@dataclass(frozen=True)
class Fallback:
    """A model-specific rung of the degrade ladder (e.g. FLUX.2's tiled VAE decode).

    `phases` are the phases whose device failure it answers; `apply(options)`
    returns the options with the fallback switched on (it must not mutate its
    argument, and its result must change the phase's `work` or behaviour,
    otherwise the rung cannot help). The manager re-plans with the new
    options and hands them to the phase function.
    """

    name: str
    phases: frozenset[str]
    apply: Callable[[Any], Any]

    def __post_init__(self) -> None:
        if not self.name:
            raise ValueError("Fallback: пустое имя")
        if not self.phases:
            raise ValueError(f"Fallback «{self.name}»: не указано ни одной фазы")


def default_key_of(family: str) -> Callable[[Any, str, str], str]:
    """The default `LargeModelSpec.key_of`: `"<family>:<format>:<device>"` (options do not change the weights)."""

    def key_of(options: Any, weight_format: str, device: str) -> str:
        return f"{family}:{weight_format}:{device}"

    return key_of


@dataclass(frozen=True)
class LargeModelSpec:
    """A whole large model as the core sees it.

    `family` keys the calibration tables (never interpreted otherwise);
    `components` in a stable order (the planner solves block residency in this
    order); `phases` in run order. Construction validates that names are unique,
    that every phase references declared components and that no phase uses a
    reserved name.

    Manager-side fields:
    - `formats(options)` -> `{format: FormatInfo}`, every weight format the
      model knows with its availability (`None` = the spec cannot be registered
      with the manager);
    - `key_of(options, format, device)` -> the residency key of the loaded
      weights (one manager handle per key): it must change whenever the loaded
      bytes would differ; `None` = `default_key_of(family)`;
    - `fallbacks`: model-specific degrade rungs in the order they are tried
      (names unique, phases declared);
    - `host_pin_of(options)` -> the names of the components THIS request pins
      to host execution (CPU, from RAM; e.g. "encoder on the CPU"); `None` =
      the model never pins. A pinned component is placed by the RAM step alone
      (`tiers.host_pinned_policy`), never on the device; see `host_pin`.
    """

    family: str
    components: tuple[ComponentSpec, ...]
    phases: tuple[PhaseSpec, ...]
    _by_name: Mapping[str, ComponentSpec] = field(init=False, repr=False, compare=False)
    formats: Callable[[Any], Mapping[str, FormatInfo]] | None = None
    key_of: Callable[[Any, str, str], str] | None = None
    fallbacks: tuple[Fallback, ...] = ()
    host_pin_of: Callable[[Any], Collection[str]] | None = None

    def __post_init__(self) -> None:
        names = [component.name for component in self.components]
        if len(set(names)) != len(names):
            raise ValueError(f"LargeModelSpec «{self.family}»: повторяющиеся имена компонентов {names}")
        if not self.phases:
            raise ValueError(f"LargeModelSpec «{self.family}»: нет ни одной фазы")
        phase_names = [phase.name for phase in self.phases]
        if len(set(phase_names)) != len(phase_names):
            raise ValueError(f"LargeModelSpec «{self.family}»: повторяющиеся имена фаз {phase_names}")
        for phase in self.phases:
            if phase.name in RESERVED_PHASE_NAMES or "->" in phase.name:
                raise ValueError(f"LargeModelSpec «{self.family}»: имя фазы «{phase.name}» зарезервировано")
            unknown = sorted(phase.uses - set(names))
            if unknown:
                raise ValueError(f"LargeModelSpec «{self.family}»: фаза «{phase.name}» ссылается на неизвестные компоненты {unknown}")
        fallback_names = [fallback.name for fallback in self.fallbacks]
        if len(set(fallback_names)) != len(fallback_names):
            raise ValueError(f"LargeModelSpec «{self.family}»: повторяющиеся имена запасных ступеней {fallback_names}")
        for fallback in self.fallbacks:
            unknown_phases = sorted(fallback.phases - set(phase_names))
            if unknown_phases:
                raise ValueError(f"LargeModelSpec «{self.family}»: запасная ступень «{fallback.name}» ссылается на неизвестные фазы {unknown_phases}")
        object.__setattr__(self, "_by_name", {component.name: component for component in self.components})

    def component(self, name: str) -> ComponentSpec:
        """The component called `name`; `KeyError` when the spec has none."""
        return self._by_name[name]

    def host_pin(self, options: Any) -> frozenset[str]:
        """The components `options` pin to host execution (`host_pin_of`; empty when the spec has none).

        # Raises
        `ValueError` when a pinned name is not a component of this spec or names
        a component that may not execute on the CPU (`exec_devices_allowed`).
        """
        if self.host_pin_of is None:
            return frozenset()
        pinned = frozenset(self.host_pin_of(options))
        for name in sorted(pinned):
            component = self._by_name.get(name)
            if component is None:
                raise ValueError(f"LargeModelSpec «{self.family}»: закреплённый за процессором компонент «{name}» не объявлен")
            if ExecDevice.CPU not in component.exec_devices_allowed:
                raise ValueError(f"LargeModelSpec «{self.family}»: компонент «{name}» не может исполняться на процессоре, закрепить его за ним нельзя")
        return pinned

    def key(self, options: Any, weight_format: str, device: str) -> str:
        """The residency key of `options` loaded in `weight_format` on `device` (`key_of` or the default).

        A request that pins components to the host (`host_pin`) gets the suffix
        `|host_pin=<names, sorted>`: its placement differs, so it is its own
        handle (with its own learned floors); toggling the pin leaves the other
        handle idle, reclaimed like any idle holding.

        # Raises
        `ValueError` from `host_pin`.
        """
        key_of = self.key_of if self.key_of is not None else default_key_of(self.family)
        key = str(key_of(options, weight_format, device))
        pinned = self.host_pin(options)
        return f"{key}|host_pin={','.join(sorted(pinned))}" if pinned else key


@dataclass(frozen=True)
class FormatInfo:
    """One weight format of a model as the planner and the UI see it.

    `available` = its files are on disk (`reason` says why not); `dit_bytes` the
    device size of its largest component, for display; `compute_path` how its
    matmuls run (`"native"`, `"scaled_mm"`, `"dequant"`) and a calibration key;
    `quality_rank` orders formats by output quality, higher = better.
    """

    available: bool
    reason: str | None
    dit_bytes: int
    compute_path: str
    quality_rank: int
