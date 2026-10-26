"""
File: modules/ai_backend/ipc/handlers/test_flux2_klein.py

Purpose:
Unit tests for the v2 IPC FLUX.2 klein handlers
(`modules/ai_backend/ipc/handlers/flux2_klein.py`), exercised by calling the
registered handlers directly with a mocked `AppState.flux2_klein_inpaint`
service (NO torch, NO weights).

Streaming mechanism under test:
The handler reads its per-request `ProgressEmitter` from the
`HandlerContext.progress_emitter` attribute (the dispatcher's streaming hook).
The tests attach a real `dispatcher.ProgressEmitter` bound to a fake dispatcher
whose `_write` records each frame, so the exact on-the-wire progress shape
(`{phase, step, total, label}` + an empty blob) is asserted.

Coverage:
- two-phase progress (`load` / `generate`) with an always-empty blob;
- terminal response: result PNG as the RESPONSE BLOB, header carrying
  `image_len`, `oom_recovered`, `applied` (`{vae_tiling}` alone) and, beside
  it (never inside), the effective `text_attention_in_mask` and the four step
  keys; the retired `placement` / `profile*` keys never reach the client;
- request blob split by `image_len`/`mask_len`/`reference_len` with strict
  equality; an absent or zero `reference_len` reaches the service as NO reference
  (`None`), a present one as exactly its own segment, and a malformed length or a
  sum that misses the blob is a request error;
- `params` passed through, `null` params treated as an empty object;
- no-emitter => no progress frames, still a correct terminal response;
- a dead peer: every frame still attempted, ONE warning per request, the work goes on;
- `inpaint.flux2_klein.status` with and without `params`;
- `inpaint.flux2_klein.estimate` is gone (protocol 14): no handler, not in
  `ALL_METHODS`;
- `inpaint.flux2_klein.unload` returns `{"unloaded": bool}`;
- error mapping: ValueError / FileNotFoundError / generic Exception propagate
  (the dispatcher maps them to `response{status:"error"}`);
- cancel before start and cancel observed after the service returns;
- `inpaint.flux2_klein.component_action`: it streams the `load` phase, requires a
  non-empty `component` and `action`, forwards both VERBATIM (the legal
  vocabulary and the per-component availability are the service's single answer,
  never re-derived here), returns the post-action snapshot, and propagates every
  refusal;
- the four `inpaint.flux2_klein.prompt_cache.*` methods (protocol 23): `build`
  streams the prompt phase, is cancellable and forwards an optional string
  `name`; `export` requires a non-empty `file` and `path`; `import` a non-empty
  `path`; the answers pass through verbatim; `.save` / `.load` are gone (no
  handler, not in `ALL_METHODS`);
- the two `inpaint.flux2_klein.download.*` methods are gone (protocol 22: the
  application's model catalog downloads the weights): no handler, not in
  `ALL_METHODS`;
- the progress forwarder: a four-positional-argument caller emits exactly the
  four-field frame, the optional byte level (`file_step` / `file_total` /
  `file_label` / `file_component`) is forwarded when given, and a dead peer is
  logged once per request without aborting the work.
"""

from __future__ import annotations

import threading
from types import SimpleNamespace
from typing import Any

import pytest

from modules.ai_backend.ipc.dispatcher import ProgressEmitter
from modules.ai_backend.ipc.protocol import (
    ALL_METHODS,
    HEADER_ID,
    HEADER_KIND,
    KIND_PROGRESS,
    METHOD_INPAINT_FLUX2_KLEIN,
    METHOD_INPAINT_FLUX2_KLEIN_COMPONENT_ACTION,
    METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_BUILD,
    METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_EXPORT,
    METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_IMPORT,
    METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_LIST,
    METHOD_INPAINT_FLUX2_KLEIN_STATUS,
    METHOD_INPAINT_FLUX2_KLEIN_UNLOAD,
)
from modules.ai_backend.ipc.registry import HandlerContext, Interrupted, get_handler

REGION_PNG = b"\x89PNG-region-bytes"
MASK_PNG = b"MASK-bytes!!"
REFERENCE_PNG = b"\x89PNG-marks-reference"
RESULT_PNG = b"\x89PNG-result-\x00\x01\x02"

REQUEST_ID = 77

#: Progress the fake service replays: one load phase, then the denoising steps.
PROGRESS_SCRIPT = (
    ("load", 1, 6, "Загрузка трансформера"),
    ("generate", 0, 4, "Генерация"),
    ("generate", 1, 4, "Генерация"),
)

#: What `prompt_cache.build` streams: the prompt phase only — it builds no
#: pipeline, so the pipeline's own load steps never occur.
BUILD_PROGRESS_SCRIPT = (
    ("load", 0, 9, "Подготовка кэширования промпта"),
    ("load", 7, 9, "Загрузка текстового энкодера"),
    ("load", 9, 9, "Выгрузка текстового энкодера"),
)

#: What `component_action` streams: the `phase:"load"` scale again — a component
#: load reads up to 18 GB and must not be a silent wait.
COMPONENT_ACTION_PROGRESS_SCRIPT = (
    ("load", 0, 9, "Подготовка загрузки FLUX.2 klein"),
    ("load", 1, 9, "Загрузка трансформера"),
    ("load", 6, 9, "Прогрев модели"),
)


class _FakeFlux2KleinService:
    """Stand-in for `AppState.flux2_klein_inpaint`."""

    def __init__(
        self,
        result: dict[str, Any] | None = None,
        *,
        raise_exc: BaseException | None = None,
    ) -> None:
        self._result = result if result is not None else _default_result()
        self._raise_exc = raise_exc
        self.calls: list[tuple[bytes, bytes, dict[str, Any]]] = []
        #: The `reference_bytes` of every `inpaint_image_bytes` call, in order.
        self.reference_calls: list[bytes | None] = []
        self.status_calls: list[Any] = []
        #: (method, params, extra kwargs) per prompt-cache call.
        self.prompt_cache_calls: list[tuple[str, Any, dict[str, Any]]] = []
        #: (params, component, action) per `component_action` call.
        self.component_action_calls: list[tuple[Any, str, str]] = []
        self.unload_calls = 0
        self.unload_return = True

    def inpaint_image_bytes(
        self,
        image_bytes: bytes,
        mask_bytes: bytes,
        *,
        reference_bytes: bytes | None,
        params: dict[str, Any],
        progress_callback: Any = None,
    ) -> dict[str, Any]:
        self.calls.append((image_bytes, mask_bytes, params))
        self.reference_calls.append(reference_bytes)
        if progress_callback is not None:
            for frame in PROGRESS_SCRIPT:
                progress_callback(*frame)
        if self._raise_exc is not None:
            raise self._raise_exc
        return self._result

    def status(self, params: dict[str, Any] | None = None) -> dict[str, Any]:
        self.status_calls.append(params)
        return {"available": False, "reason": "Не выбран трансформер"}

    def unload(self) -> bool:
        self.unload_calls += 1
        return self.unload_return

    def component_action(
        self,
        params: dict[str, Any] | None,
        *,
        component: str,
        action: str,
        progress_callback: Any = None,
    ) -> dict[str, Any]:
        self.component_action_calls.append((params, component, action))
        if progress_callback is not None:
            for frame in COMPONENT_ACTION_PROGRESS_SCRIPT:
                progress_callback(*frame)
        if self._raise_exc is not None:
            raise self._raise_exc
        return {
            "component": component,
            "action": action,
            "performed": True,
            "components": {
                "text_encoder": {"residency": "not_loaded", "actions": ["load"]},
                "transformer": {"residency": "gpu", "actions": ["unload", "to_ram"]},
                "vae": {"residency": "gpu", "actions": ["unload", "warmup"]},
            },
            "components_busy": False,
            "device": "cuda:0",
        }

    # ---- prompt-cache library ----
    def prompt_cache_build(
        self, params: dict[str, Any] | None, *, name: str | None = None, progress_callback: Any = None
    ) -> dict[str, Any]:
        self.prompt_cache_calls.append(("build", params, {"name": name}))
        if progress_callback is not None:
            for frame in BUILD_PROGRESS_SCRIPT:
                progress_callback(*frame)
        if self._raise_exc is not None:
            raise self._raise_exc
        return {"prompt": "clean", "name": name or "clean", "encoded": True, "written": [], "renamed": False, "prompt_cached": True, "device": "cuda:0"}

    def prompt_cache_list(self, params: dict[str, Any] | None) -> dict[str, Any]:
        self.prompt_cache_calls.append(("list", params, {}))
        return {"library": "flux2_klein_9b", "directory": "/x", "texts": [], "skipped": [], "text_encoder_available": True, "legacy": None}

    def prompt_cache_export(self, params: dict[str, Any] | None, file: str, path: str) -> dict[str, Any]:
        self.prompt_cache_calls.append(("export", params, {"file": file, "path": path}))
        return {"library": "flux2_klein_9b", "file": file, "path": path, "size_bytes": 42}

    def prompt_cache_import(self, params: dict[str, Any] | None, path: str) -> dict[str, Any]:
        self.prompt_cache_calls.append(("import", params, {"path": path}))
        return {"library": "flux2_klein_4b", "file": "theirs-0123456789-abcdef01.msprompt", "prompt": "clean", "name": "theirs", "library_matches": False, "already_present": False, "origin": "imported"}


class _RecordingDispatcher:
    """Minimal fake dispatcher exposing `_write` so a real `ProgressEmitter`
    can record the frames it would put on the wire."""

    def __init__(self) -> None:
        self.frames: list[tuple[dict[str, Any], bytes]] = []

    def _write(self, header: dict[str, Any], blob: bytes = b"") -> None:
        self.frames.append((dict(header), blob))


def _default_result() -> dict[str, Any]:
    return {
        "image_png": RESULT_PNG,
        "region_size": [512, 512],
        "device": "cuda:0",
        "oom_recovered": False,
        "applied": {"vae_tiling": True},
        "vram_step": "phase",
        "ram_step": "keep",
        "vram_step_effective": "phase",
        "ram_step_effective": "keep",
        "degraded": False,
        "degrade_reason": None,
    }


def _ctx(svc: _FakeFlux2KleinService, *, emitter: Any = None) -> HandlerContext:
    state = SimpleNamespace(flux2_klein_inpaint=svc)
    ctx = HandlerContext(state=state, events=None, get_health_snapshot=lambda: {})
    if emitter is not None:
        ctx.progress_emitter = emitter  # type: ignore[attr-defined]
    return ctx


def _header(extra: dict[str, Any] | None = None) -> dict[str, Any]:
    header: dict[str, Any] = {"image_len": len(REGION_PNG), "mask_len": len(MASK_PNG)}
    if extra:
        header.update(extra)
    return header


def _no_cancel() -> threading.Event:
    return threading.Event()


def _emitter() -> tuple[ProgressEmitter, _RecordingDispatcher]:
    disp = _RecordingDispatcher()
    return ProgressEmitter(disp, REQUEST_ID), disp


# ---------------------------------------------------------------------------
# Streaming + terminal response
# ---------------------------------------------------------------------------
def test_streaming_emits_two_phases_then_the_terminal_response() -> None:
    svc = _FakeFlux2KleinService()
    emitter, disp = _emitter()
    ctx = _ctx(svc, emitter=emitter)
    params = {"prompt": "clean background", "steps": 4}

    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN)
    resp_header, resp_blob = handler(
        ctx, _header({"params": params}), REGION_PNG + MASK_PNG, _no_cancel()
    )

    assert len(disp.frames) == len(PROGRESS_SCRIPT)
    for (frame_header, frame_blob), expected in zip(disp.frames, PROGRESS_SCRIPT):
        phase, step, total, label = expected
        assert frame_header[HEADER_KIND] == KIND_PROGRESS
        assert frame_header[HEADER_ID] == REQUEST_ID
        assert frame_header["phase"] == phase
        assert frame_header["step"] == step
        assert frame_header["total"] == total
        assert frame_header["label"] == label
        assert frame_blob == b""  # this method never carries a preview

    assert svc.calls == [(REGION_PNG, MASK_PNG, params)]
    assert resp_blob == RESULT_PNG
    assert resp_header == {
        "image_len": len(RESULT_PNG),
        "oom_recovered": False,
        "applied": {"vae_tiling": True},
        "text_attention_in_mask": False,
        "vram_step": "phase",
        "ram_step": "keep",
        "vram_step_effective": "phase",
        "ram_step_effective": "keep",
        "degraded": False,
        "degrade_reason": None,
    }
    assert "image_png" not in resp_header


def test_a_degraded_run_reports_the_manager_fields() -> None:
    result = _default_result()
    result.update({"vram_step": "max", "vram_step_effective": "phase", "degraded": True, "degrade_reason": "device_oom"})
    svc = _FakeFlux2KleinService(result)
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN)
    resp_header, _blob = handler(_ctx(svc), _header({"params": {}}), REGION_PNG + MASK_PNG, _no_cancel())
    assert (resp_header["degraded"], resp_header["degrade_reason"]) == (True, "device_oom")
    assert (resp_header["vram_step"], resp_header["vram_step_effective"], resp_header["ram_step"], resp_header["ram_step_effective"]) == ("max", "phase", "keep", "keep")


def test_the_step_keys_mirror_the_service() -> None:
    from modules.ai_backend.inpaint.flux2_klein import memory
    from modules.ai_backend.ipc.handlers import flux2_klein as handler_module

    assert handler_module._STEP_KEYS == memory.STEP_FIELD_KEYS


def test_oom_recovery_is_reported_in_the_response_header() -> None:
    result = _default_result()
    result["oom_recovered"] = True
    svc = _FakeFlux2KleinService(result)
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN)
    resp_header, _blob = handler(
        _ctx(svc), _header({"params": {}}), REGION_PNG + MASK_PNG, _no_cancel()
    )
    assert resp_header["oom_recovered"] is True
    assert resp_header["applied"] == {"vae_tiling": True}


def test_applied_is_exactly_the_tiling_flag_whatever_the_service_answers() -> None:
    # The client persists `applied`; a flag the service omits reads `false`, and
    # an extra key (a retired v13 flag) never reaches the wire.
    result = _default_result()
    result["applied"] = {"unload_transformer_before_vae": True}
    result["placement"] = "full_gpu"
    result["profile"] = "max_speed"
    svc = _FakeFlux2KleinService(result)
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN)
    resp_header, _blob = handler(
        _ctx(svc), _header({"params": {}}), REGION_PNG + MASK_PNG, _no_cancel()
    )
    assert resp_header["applied"] == {"vae_tiling": False}
    for retired in ("placement", "profile", "profile_effective"):
        assert retired not in resp_header


def test_the_effective_text_attention_flag_is_reported_beside_applied() -> None:
    # Beside `applied`, never inside it: `applied` is persisted into the user's
    # settings, and a `whole_region` run forcing the flag off must not switch
    # the user's choice off for every later run.
    result = _default_result()
    result["text_attention_in_mask"] = True
    svc = _FakeFlux2KleinService(result)
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN)
    resp_header, _blob = handler(
        _ctx(svc), _header({"params": {}}), REGION_PNG + MASK_PNG, _no_cancel()
    )
    assert resp_header["text_attention_in_mask"] is True
    assert "text_attention_in_mask" not in resp_header["applied"]


def test_no_emitter_still_returns_the_terminal_response() -> None:
    svc = _FakeFlux2KleinService()
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN)
    _resp_header, resp_blob = handler(
        _ctx(svc), _header({"params": {}}), REGION_PNG + MASK_PNG, _no_cancel()
    )
    assert resp_blob == RESULT_PNG


def test_null_params_are_treated_as_an_empty_object() -> None:
    svc = _FakeFlux2KleinService()
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN)
    handler(_ctx(svc), _header({"params": None}), REGION_PNG + MASK_PNG, _no_cancel())
    assert svc.calls[0][2] == {}


def test_non_object_params_are_a_request_error() -> None:
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN)
    with pytest.raises(ValueError):
        handler(
            _ctx(_FakeFlux2KleinService()),
            _header({"params": [1, 2]}),
            REGION_PNG + MASK_PNG,
            _no_cancel(),
        )


# ---------------------------------------------------------------------------
# Blob splitting
# ---------------------------------------------------------------------------
@pytest.mark.parametrize(
    "header",
    [
        {"image_len": 1, "mask_len": 1},  # sum != blob length
        {"image_len": -1, "mask_len": len(MASK_PNG)},
        {"image_len": True, "mask_len": len(MASK_PNG)},
        {"mask_len": len(MASK_PNG)},  # image_len missing
        {"image_len": len(REGION_PNG)},  # mask_len missing
    ],
)
def test_a_bad_blob_split_is_a_request_error(header: dict[str, Any]) -> None:
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN)
    with pytest.raises(ValueError):
        handler(
            _ctx(_FakeFlux2KleinService()), header, REGION_PNG + MASK_PNG, _no_cancel()
        )


@pytest.mark.parametrize(
    "header",
    [
        # One byte short of covering the reference, and one byte over.
        {"reference_len": len(REFERENCE_PNG) - 1},
        {"reference_len": len(REFERENCE_PNG) + 1},
        {"reference_len": -1},
        {"reference_len": True},
        {"reference_len": "17"},
        {"reference_len": 1.5},
    ],
)
def test_a_bad_reference_length_is_a_request_error(header: dict[str, Any]) -> None:
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN)
    svc = _FakeFlux2KleinService()
    with pytest.raises(ValueError):
        handler(_ctx(svc), _header(header), REGION_PNG + MASK_PNG + REFERENCE_PNG, _no_cancel())
    assert svc.calls == [], "a mis-sliced request never reaches the service"


def test_a_reference_beside_an_undeclared_length_is_a_request_error() -> None:
    # The trailing bytes are not silently ignored: without `reference_len` the two
    # declared lengths no longer sum to the blob.
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN)
    with pytest.raises(ValueError):
        handler(
            _ctx(_FakeFlux2KleinService()),
            _header(),
            REGION_PNG + MASK_PNG + REFERENCE_PNG,
            _no_cancel(),
        )


def test_a_declared_reference_reaches_the_service_as_its_own_segment() -> None:
    svc = _FakeFlux2KleinService()
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN)
    handler(
        _ctx(svc),
        _header({"reference_len": len(REFERENCE_PNG), "params": {}}),
        REGION_PNG + MASK_PNG + REFERENCE_PNG,
        _no_cancel(),
    )
    assert svc.calls == [(REGION_PNG, MASK_PNG, {})]
    assert svc.reference_calls == [REFERENCE_PNG]


@pytest.mark.parametrize("extra", [{}, {"reference_len": 0}, {"reference_len": None}])
def test_an_absent_or_zero_reference_length_means_no_reference(extra: dict[str, Any]) -> None:
    svc = _FakeFlux2KleinService()
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN)
    handler(_ctx(svc), _header(extra), REGION_PNG + MASK_PNG, _no_cancel())
    assert svc.reference_calls == [None]


def test_an_empty_mask_is_refused() -> None:
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN)
    with pytest.raises(ValueError):
        handler(
            _ctx(_FakeFlux2KleinService()),
            {"image_len": len(REGION_PNG), "mask_len": 0},
            REGION_PNG,
            _no_cancel(),
        )


# ---------------------------------------------------------------------------
# Errors and cancellation
# ---------------------------------------------------------------------------
@pytest.mark.parametrize(
    "exc", [ValueError("bad region"), FileNotFoundError("vae missing"), RuntimeError("boom")]
)
def test_service_failures_propagate(exc: BaseException) -> None:
    svc = _FakeFlux2KleinService(raise_exc=exc)
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN)
    with pytest.raises(type(exc)):
        handler(_ctx(svc), _header({"params": {}}), REGION_PNG + MASK_PNG, _no_cancel())


def test_cancel_before_start_is_an_interrupt() -> None:
    cancel = threading.Event()
    cancel.set()
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN)
    with pytest.raises(Interrupted):
        handler(
            _ctx(_FakeFlux2KleinService()),
            _header({"params": {}}),
            REGION_PNG + MASK_PNG,
            cancel,
        )


def test_cancel_observed_after_the_service_returns_is_an_interrupt() -> None:
    cancel = threading.Event()
    svc = _FakeFlux2KleinService()

    class _CancellingService(_FakeFlux2KleinService):
        def inpaint_image_bytes(self, *args: Any, **kwargs: Any) -> dict[str, Any]:
            result = super().inpaint_image_bytes(*args, **kwargs)
            cancel.set()
            return result

    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN)
    with pytest.raises(Interrupted):
        handler(
            _ctx(_CancellingService(svc._result)),
            _header({"params": {}}),
            REGION_PNG + MASK_PNG,
            cancel,
        )


# ---------------------------------------------------------------------------
# status / unload
# ---------------------------------------------------------------------------
def test_status_forwards_params_when_given() -> None:
    svc = _FakeFlux2KleinService()
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN_STATUS)
    header, blob = handler(_ctx(svc), {"params": {"vae_path": "/models/vae"}}, b"", _no_cancel())
    assert blob == b""
    assert header["available"] is False
    assert svc.status_calls == [{"vae_path": "/models/vae"}]


def test_status_without_params_asks_for_the_last_configuration() -> None:
    svc = _FakeFlux2KleinService()
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN_STATUS)
    handler(_ctx(svc), {}, b"", _no_cancel())
    assert svc.status_calls == [None]


def test_the_estimate_method_is_gone() -> None:
    # Protocol 14: forecasts are `large_models.profiles`; the dispatcher answers
    # the old name as an unknown method.
    assert get_handler("inpaint.flux2_klein.estimate") is None
    assert "inpaint.flux2_klein.estimate" not in ALL_METHODS


@pytest.mark.parametrize("unloaded", [True, False])
def test_unload_reports_whether_anything_was_dropped(unloaded: bool) -> None:
    svc = _FakeFlux2KleinService()
    svc.unload_return = unloaded
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN_UNLOAD)
    header, blob = handler(_ctx(svc), {}, b"", _no_cancel())
    assert header == {"unloaded": unloaded}
    assert blob == b""
    assert svc.unload_calls == 1


# ---------------------------------------------------------------------------
# Per-component residency actions
# ---------------------------------------------------------------------------
def test_component_action_streams_the_load_phase_and_returns_the_snapshot() -> None:
    svc = _FakeFlux2KleinService()
    emitter, disp = _emitter()
    params = {"transformer_path": "/t", "vae_path": "/v"}

    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN_COMPONENT_ACTION)
    header, blob = handler(
        _ctx(svc, emitter=emitter),
        {"params": params, "component": "transformer", "action": "load"},
        b"",
        _no_cancel(),
    )

    assert [(f[0]["phase"], f[0]["step"]) for f in disp.frames] == [
        (phase, step) for phase, step, _total, _label in COMPONENT_ACTION_PROGRESS_SCRIPT
    ]
    assert all(frame_blob == b"" for _h, frame_blob in disp.frames)
    assert svc.component_action_calls == [(params, "transformer", "load")]
    assert header["components_busy"] is False
    assert header["components"]["transformer"]["actions"] == ["unload", "to_ram"]
    assert blob == b""


def test_component_action_without_an_emitter_still_answers() -> None:
    svc = _FakeFlux2KleinService()
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN_COMPONENT_ACTION)
    header, _blob = handler(
        _ctx(svc), {"params": None, "component": "vae", "action": "warmup"}, b"", _no_cancel()
    )
    assert header["performed"] is True
    assert svc.component_action_calls == [({}, "vae", "warmup")]


@pytest.mark.parametrize("field", ["component", "action"])
@pytest.mark.parametrize("value", [None, "", "   ", 7])
def test_component_action_requires_both_names(field: str, value: Any) -> None:
    header: dict[str, Any] = {"params": {}, "component": "vae", "action": "warmup"}
    if value is None:
        header.pop(field)
    else:
        header[field] = value
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN_COMPONENT_ACTION)
    with pytest.raises(ValueError):
        handler(_ctx(_FakeFlux2KleinService()), header, b"", _no_cancel())


def test_component_action_does_not_second_guess_the_service_vocabulary() -> None:
    # The handler forwards whatever arrived; which names are legal, and which are
    # possible right now, is the service's single answer.
    svc = _FakeFlux2KleinService()
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN_COMPONENT_ACTION)
    handler(
        _ctx(svc), {"params": {}, "component": "tokenizer", "action": "to_gpu"}, b"", _no_cancel()
    )
    assert svc.component_action_calls == [({}, "tokenizer", "to_gpu")]


@pytest.mark.parametrize(
    "exc", [ValueError("не в списке доступных"), RuntimeError("FLUX.2 klein занят")]
)
def test_a_refused_component_action_propagates(exc: BaseException) -> None:
    svc = _FakeFlux2KleinService(raise_exc=exc)
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN_COMPONENT_ACTION)
    with pytest.raises(type(exc)):
        handler(
            _ctx(svc),
            {"params": {}, "component": "text_encoder", "action": "load"},
            b"",
            _no_cancel(),
        )


def test_component_action_honours_cancellation_before_start() -> None:
    svc = _FakeFlux2KleinService()
    cancel = threading.Event()
    cancel.set()
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN_COMPONENT_ACTION)
    with pytest.raises(Interrupted):
        handler(
            _ctx(svc), {"params": {}, "component": "vae", "action": "warmup"}, b"", cancel
        )
    assert svc.component_action_calls == []


# ---------------------------------------------------------------------------
# Prompt-cache library
# ---------------------------------------------------------------------------
def test_prompt_cache_build_streams_the_prompt_phase() -> None:
    svc = _FakeFlux2KleinService()
    emitter, disp = _emitter()
    params = {"prompt": "clean background", "prompt_library": "flux2_klein_9b"}

    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_BUILD)
    header, blob = handler(_ctx(svc, emitter=emitter), {"params": params}, b"", _no_cancel())

    assert [(f[0]["phase"], f[0]["step"]) for f in disp.frames] == [
        (phase, step) for phase, step, _total, _label in BUILD_PROGRESS_SCRIPT
    ]
    assert all(frame_blob == b"" for _h, frame_blob in disp.frames)
    assert svc.prompt_cache_calls == [("build", params, {"name": None})]
    assert header["prompt_cached"] is True
    assert blob == b""


def test_prompt_cache_build_without_an_emitter_still_answers() -> None:
    svc = _FakeFlux2KleinService()
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_BUILD)
    header, _blob = handler(_ctx(svc), {"params": None}, b"", _no_cancel())
    assert header["encoded"] is True
    assert svc.prompt_cache_calls == [("build", {}, {"name": None})]


@pytest.mark.parametrize("name", ["Кот", "", "  spaced  "])
def test_prompt_cache_build_forwards_the_name_verbatim(name: str) -> None:
    svc = _FakeFlux2KleinService()
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_BUILD)
    header, _blob = handler(_ctx(svc), {"params": {}, "name": name}, b"", _no_cancel())
    assert svc.prompt_cache_calls == [("build", {}, {"name": name})]
    assert header["name"] == (name or "clean")


@pytest.mark.parametrize("bad_name", [7, [], {}, True])
def test_prompt_cache_build_refuses_a_non_string_name(bad_name: Any) -> None:
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_BUILD)
    with pytest.raises(ValueError):
        handler(_ctx(_FakeFlux2KleinService()), {"params": {}, "name": bad_name}, b"", _no_cancel())


def test_prompt_cache_build_honours_cancellation_before_start() -> None:
    svc = _FakeFlux2KleinService()
    cancel = threading.Event()
    cancel.set()
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_BUILD)
    with pytest.raises(Interrupted):
        handler(_ctx(svc), {"params": {}}, b"", cancel)
    assert svc.prompt_cache_calls == []


def test_prompt_cache_list_passes_the_params_through() -> None:
    svc = _FakeFlux2KleinService()
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_LIST)
    params = {"text_encoder_path": "/enc", "prompt_library": "flux2_klein_9b"}
    header, blob = handler(_ctx(svc), {"params": params}, b"", _no_cancel())
    assert header["library"] == "flux2_klein_9b"
    assert header["texts"] == []
    assert header["legacy"] is None
    assert blob == b""
    assert svc.prompt_cache_calls == [("list", params, {})]


def test_prompt_cache_export_requires_both_a_file_and_a_path() -> None:
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_EXPORT)
    for header in ({"params": {}, "file": "n.msprompt"}, {"params": {}, "path": "/x.msprompt"}, {"params": {}, "file": " ", "path": "/x.msprompt"}, {"params": {}, "name": "n", "path": "/x.msprompt"}):
        with pytest.raises(ValueError):
            handler(_ctx(_FakeFlux2KleinService()), header, b"", _no_cancel())

    svc = _FakeFlux2KleinService()
    header, _blob = handler(_ctx(svc), {"params": {}, "file": "n.msprompt", "path": "/x.msprompt"}, b"", _no_cancel())
    assert header == {"library": "flux2_klein_9b", "file": "n.msprompt", "path": "/x.msprompt", "size_bytes": 42}
    assert svc.prompt_cache_calls == [("export", {}, {"file": "n.msprompt", "path": "/x.msprompt"})]


def test_prompt_cache_import_passes_the_answer_through() -> None:
    svc = _FakeFlux2KleinService()
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_IMPORT)
    header, _blob = handler(_ctx(svc), {"params": {}, "path": "/in/theirs.msprompt"}, b"", _no_cancel())
    # The Rust side warns on another library; the import itself is not an error.
    assert header["library_matches"] is False
    assert header["library"] == "flux2_klein_4b"
    assert svc.prompt_cache_calls == [("import", {}, {"path": "/in/theirs.msprompt"})]
    with pytest.raises(ValueError):
        handler(_ctx(_FakeFlux2KleinService()), {"params": {}}, b"", _no_cancel())


def test_every_prompt_cache_method_is_registered() -> None:
    for method in (
        METHOD_INPAINT_FLUX2_KLEIN_COMPONENT_ACTION,
        METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_BUILD,
        METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_LIST,
        METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_EXPORT,
        METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_IMPORT,
    ):
        assert get_handler(method) is not None


@pytest.mark.parametrize("method", ["inpaint.flux2_klein.prompt_cache.save", "inpaint.flux2_klein.prompt_cache.load"])
def test_the_save_and_load_methods_are_gone(method: str) -> None:
    # Protocol 23: a preset is saved by `.build` and read by the run itself.
    assert get_handler(method) is None
    assert method not in ALL_METHODS


# ---------------------------------------------------------------------------
# Progress forwarding and retired methods
# ---------------------------------------------------------------------------
def test_existing_four_argument_progress_callers_are_unchanged() -> None:
    # The byte level is additive: a service that calls its progress callback
    # with four positional arguments must emit exactly the four-field frame.
    from modules.ai_backend.ipc.handlers.streaming import progress_forwarder

    emitter, disp = _emitter()
    forward = progress_forwarder(_ctx(_FakeFlux2KleinService(), emitter=emitter), METHOD_INPAINT_FLUX2_KLEIN)
    forward("load", 2, 6, "Загрузка трансформера")

    assert len(disp.frames) == 1
    frame_header, frame_blob = disp.frames[0]
    assert frame_blob == b""
    assert frame_header["phase"] == "load"
    assert frame_header["step"] == 2
    assert frame_header["total"] == 6
    assert frame_header["label"] == "Загрузка трансформера"
    assert not {"file_step", "file_total", "file_label", "file_component"} & set(frame_header)


def test_the_forwarder_sends_the_component_of_a_load_byte_level() -> None:
    from modules.ai_backend.ipc.handlers.streaming import progress_forwarder

    emitter, disp = _emitter()
    forward = progress_forwarder(_ctx(_FakeFlux2KleinService(), emitter=emitter), METHOD_INPAINT_FLUX2_KLEIN)
    forward("load", 7, 9, "Загрузка текстового энкодера", file_step=5, file_total=10, file_label="text_encoder", file_component="text_encoder")

    frame_header, _blob = disp.frames[0]
    assert (frame_header["step"], frame_header["file_step"], frame_header["file_total"]) == (7, 5, 10)
    assert frame_header["file_label"] == "text_encoder"
    assert frame_header["file_component"] == "text_encoder"


@pytest.mark.parametrize("method", ["inpaint.flux2_klein.download.check", "inpaint.flux2_klein.download.start"])
def test_the_download_methods_are_gone(method: str) -> None:
    # Protocol 22: the application's model catalog downloads every FLUX.2 klein
    # weight; the dispatcher answers the old names as unknown methods.
    assert get_handler(method) is None
    assert method not in ALL_METHODS


def test_a_dead_peer_is_logged_once_per_request_and_never_aborts_the_work(caplog: pytest.LogCaptureFixture) -> None:
    # Documented semantics: the work goes on (a disconnect does not cancel a
    # run), but the failure is never swallowed silently.
    from modules.ai_backend.ipc.handlers.streaming import progress_forwarder

    class _DeadDispatcher:
        def __init__(self) -> None:
            self.attempts = 0

        def _write(self, header: dict[str, Any], blob: bytes = b"") -> None:
            self.attempts += 1
            raise BrokenPipeError("peer gone")

    dead = _DeadDispatcher()
    forward = progress_forwarder(_ctx(_FakeFlux2KleinService(), emitter=ProgressEmitter(dead, REQUEST_ID)), METHOD_INPAINT_FLUX2_KLEIN)
    with caplog.at_level("WARNING", logger="modules.ai_backend.ipc.handlers.streaming"):
        for step in range(3):
            forward("generate", step, 3, "Генерация")
    assert dead.attempts == 3, "every frame is still attempted"
    warnings = [record for record in caplog.records if record.levelname == "WARNING"]
    assert len(warnings) == 1
    assert METHOD_INPAINT_FLUX2_KLEIN in warnings[0].getMessage()
    assert "BrokenPipeError" in warnings[0].getMessage()
