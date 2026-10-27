"""
File: modules/ai_backend/ipc/handlers/large_models.py

Methods hosted here (introduced in protocol v13, `PROTOCOL.md §5.10`):
    large_models.profiles       — the VRAM / RAM step pickers planned for one request (read-only).
    large_models.limits.set     — the pool's hard VRAM / RAM caps (persisted).
    large_models.status         — loaded handles + the limits in force.
    large_models.unload         — drop the idle handles (all, or one model's).
    large_models.learned.reset  — forget the learned step floors, both axes (all, or one model's).

All five reach `ctx.state.large_models` (`runtime/large_models/manager.py`
`LargeModelManager`). This layer validates field types, builds the
model-agnostic `RequestShape` and the requested `Steps` («Видеопамять» x
«Оперативная память», each `auto` by default), and forwards; what a step, a
format or a limit means is the manager's single answer. Fields of the retired
single profile scale (`profile`, `keep_policy`) are refused by name, so a stale
client gets a clear error instead of a silently ignored choice.

Adapter contract (`AppState.large_model_adapters`): `large_models.profiles`
names a model registered with the manager; turning that model's wire `params`
(paths, dtype, options) into the manager's `options` object and the torch
device is the model service's business, so the handler looks up
`ctx.state.large_model_adapters[model]` and calls
`adapter.large_model_inputs(params: dict) -> (options, device: str)`, which
raises `ValueError` for invalid params. A model with no adapter is refused.
An adapter whose params select one of several registrations (FLUX.2: the
`dtype` picks `flux2_klein` or `flux2_klein.float16`) also implements
`large_model_name(params: dict) -> str`; the forecast is then planned under
that registered name instead of the wire `model`. An adapter whose operation
opens SEVERAL handles (FLUX.2: pipeline + text encoder) implements
`large_model_profiles(params: dict, shape: RequestShape, *, format: str | None,
steps: Steps) -> dict` and composes the whole answer itself (one
`ProfilesReport.to_dict()`-shaped dict; a pipeline + text-encoder service
builds it with `runtime.large_models.paired.plan_pair(...).to_wire()`: the
pipeline planned with the encoder's admission claimed, plus the additive
`encoder` / `encoder_formats` keys); when present it is preferred, and
`large_model_inputs` / `large_model_name` are not called.

Torch-free at import: `runtime/large_models/{spec,tiers}.py` are stdlib-only.
"""

from __future__ import annotations

import threading
from typing import Any, TypeVar

from ...runtime.large_models.spec import RequestShape
from ...runtime.large_models.tiers import RamStep, Steps, VramStep
from ..protocol import (
    METHOD_LARGE_MODELS_LEARNED_RESET,
    METHOD_LARGE_MODELS_LIMITS_SET,
    METHOD_LARGE_MODELS_PROFILES,
    METHOD_LARGE_MODELS_STATUS,
    METHOD_LARGE_MODELS_UNLOAD,
)
from ..registry import HandlerContext, register

#: `large_models.limits.set` fields -> the `LargeModelManager.set_limits` keyword they fill.
_CAP_FIELDS = (("vram_cap_bytes", "vram_cap"), ("ram_cap_bytes", "ram_cap"))

#: One of the two step axes (`_read_step` answers a member of the enum it was given).
_StepT = TypeVar("_StepT", VramStep, RamStep)


def _optional_str(header: dict[str, Any], field: str) -> str | None:
    """Read an optional string field; absent or `null` reads as `None`, an empty string is refused."""
    value = header.get(field)
    if value is None:
        return None
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"Field '{field}' must be a non-empty string or null.")
    return value


def _require_str(header: dict[str, Any], field: str) -> str:
    """Read a required non-empty string field."""
    value = _optional_str(header, field)
    if value is None:
        raise ValueError(f"Field '{field}' is required.")
    return value


def _optional_int(container: dict[str, Any], field: str, *, minimum: int) -> int | None:
    """Read an optional integer >= `minimum` from `container`; absent or `null` is `None`."""
    value = container.get(field)
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        raise ValueError(f"Field '{field}' must be an integer >= {minimum}.")
    return value


def _require_int(container: dict[str, Any], field: str, *, minimum: int) -> int:
    """Read a required integer >= `minimum` from `container`."""
    value = _optional_int(container, field, minimum=minimum)
    if value is None:
        raise ValueError(f"Field '{field}' is required.")
    return value


def _read_params(header: dict[str, Any]) -> dict[str, Any]:
    """The `params` object; absent or `null` is an empty object."""
    params = header.get("params")
    if params is None:
        return {}
    if not isinstance(params, dict):
        raise ValueError("Field 'params' must be an object.")
    return params


def _read_shape(header: dict[str, Any]) -> RequestShape:
    """The `shape` object as a `RequestShape` (`region_width/height`, `references: [{w, h}]`, `prompt_tokens?`, `steps?`)."""
    shape = header.get("shape")
    if not isinstance(shape, dict):
        raise ValueError("Field 'shape' must be an object.")
    width = _require_int(shape, "region_width", minimum=1)
    height = _require_int(shape, "region_height", minimum=1)
    raw_references = shape.get("references")
    if raw_references is None:
        raw_references = []
    if not isinstance(raw_references, list):
        raise ValueError("Field 'shape.references' must be an array of {w, h} objects.")
    references: list[tuple[int, int]] = []
    for index, item in enumerate(raw_references):
        if not isinstance(item, dict):
            raise ValueError(f"Field 'shape.references[{index}]' must be an object {{w, h}}.")
        references.append((_require_int(item, "w", minimum=1), _require_int(item, "h", minimum=1)))
    return RequestShape(
        width=width,
        height=height,
        references=tuple(references),
        prompt_tokens=_optional_int(shape, "prompt_tokens", minimum=0),
        steps=_optional_int(shape, "steps", minimum=1),
    )


def _refuse_retired(header: dict[str, Any], field: str, replacement: str) -> None:
    """Refuse a field of the retired single profile scale, naming what replaced it."""
    if field in header:
        raise ValueError(f"Field '{field}' is no longer supported; {replacement}.")


def _read_step(header: dict[str, Any], field: str, kind: type[_StepT]) -> _StepT:
    """An optional step field as a `kind` member; absent or `null` is `kind.AUTO`."""
    raw = _optional_str(header, field)
    if raw is None:
        return kind.AUTO
    try:
        return kind(raw)
    except ValueError:
        raise ValueError(f"Field '{field}' must be one of {[step.value for step in kind]}, got {raw!r}.") from None


def _read_steps(header: dict[str, Any]) -> Steps:
    """The requested `Steps` from `vram_step` / `ram_step` (each defaults to `auto`)."""
    _refuse_retired(header, "profile", "send 'vram_step' and 'ram_step' instead")
    return Steps(vram=_read_step(header, "vram_step", VramStep), ram=_read_step(header, "ram_step", RamStep))


def _handle_profiles(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`large_models.profiles`: `LargeModelManager.profiles(...).to_dict()` for the requested steps, unchanged (or the adapter's `large_model_profiles` answer).

    `selected` is what the requested pair resolves to; `vram.steps` / `ram.steps`
    are the two picker rows (each planned with the other axis' selected step).
    """
    model = _require_str(header, "model")
    params = _read_params(header)
    shape = _read_shape(header)
    weight_format = _optional_str(header, "format")
    steps = _read_steps(header)
    adapter = ctx.state.large_model_adapters.get(model)
    if adapter is None:
        raise ValueError(f"Large model {model!r} is not served by large_models.profiles; known: {sorted(ctx.state.large_model_adapters)}.")
    # A multi-handle operation composes its own answer (module header).
    compose = getattr(adapter, "large_model_profiles", None)
    if callable(compose):
        return dict(compose(params, shape, format=weight_format, steps=steps)), b""
    options, device = adapter.large_model_inputs(params)
    # The params may select a different registration of the same family (e.g. a dtype variant).
    name_of = getattr(adapter, "large_model_name", None)
    registered = name_of(params) if callable(name_of) else model
    return ctx.state.large_models.profiles(registered, options, shape, device=device, format=weight_format, steps=steps).to_dict(), b""


def _handle_limits_set(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`large_models.limits.set`: an ABSENT field keeps its value, `null` clears it (no cap).

    Answers `{"limits": {vram_cap, ram_cap}}`: the limits this call put in
    force (`LargeModelManager.set_limits` merges the present fields with the
    limits in force atomically). A `keep_policy` field is refused
    (the steps decide what stays between runs). A store failure leaves the
    limits unchanged and is a request error.
    """
    _refuse_retired(header, "keep_policy", "the VRAM / RAM steps decide what stays between runs")
    manager = ctx.state.large_models
    # Only the fields present are passed: the manager merges them with the
    # limits in force under its own lock (an absent keyword keeps its cap).
    values: dict[str, int | None] = {}
    for field, keyword in _CAP_FIELDS:
        if field in header:
            values[keyword] = _optional_int(header, field, minimum=0)
    limits = manager.set_limits(**values)
    return {"limits": {"vram_cap": limits.vram_cap, "ram_cap": limits.ram_cap}}, b""


def _handle_status(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`large_models.status`: `{large_models: [...], limits: {...}}` (`LargeModelManager.health()`, no probe)."""
    return dict(ctx.state.large_models.health()), b""


def _handle_unload(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`large_models.unload`: `{unloaded: [keys]}`; a handle in a session is skipped (not an error)."""
    model = _optional_str(header, "model")
    return {"unloaded": list(ctx.state.large_models.unload(model))}, b""


def _handle_learned_reset(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`large_models.learned.reset`: `{cleared: int}`, the number of learned floor entries forgotten (each holds both axes)."""
    model = _optional_str(header, "model")
    return {"cleared": int(ctx.state.large_models.reset_floors(model))}, b""


register(METHOD_LARGE_MODELS_PROFILES, _handle_profiles)
register(METHOD_LARGE_MODELS_LIMITS_SET, _handle_limits_set)
register(METHOD_LARGE_MODELS_STATUS, _handle_status)
register(METHOD_LARGE_MODELS_UNLOAD, _handle_unload)
register(METHOD_LARGE_MODELS_LEARNED_RESET, _handle_learned_reset)
