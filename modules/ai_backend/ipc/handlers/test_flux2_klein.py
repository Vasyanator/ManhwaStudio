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
  `image_len`, `oom_recovered` and the `applied` memory settings;
- request blob split by `image_len`/`mask_len` with strict equality;
- `params` passed through, `null` params treated as an empty object;
- no-emitter => no progress frames, still a correct terminal response;
- `inpaint.flux2_klein.status` with and without `params`;
- `inpaint.flux2_klein.estimate` requires a positive region size;
- `inpaint.flux2_klein.unload` returns `{"unloaded": bool}`;
- error mapping: ValueError / FileNotFoundError / generic Exception propagate
  (the dispatcher maps them to `response{status:"error"}`);
- cancel before start and cancel observed after the service returns;
- the six `inpaint.flux2_klein.prompt_cache.*` methods: `build` streams the
  prompt phase and is cancellable, `name`/`path` are required non-empty strings,
  `overwrite` defaults to `false`, an import's `name` is optional and trimmed,
  and the import answer carries `family_matches` for the foreign-family warning.
"""

from __future__ import annotations

import threading
from types import SimpleNamespace
from typing import Any

import pytest

from modules.ai_backend.ipc.dispatcher import ProgressEmitter
from modules.ai_backend.ipc.protocol import (
    HEADER_ID,
    HEADER_KIND,
    KIND_PROGRESS,
    METHOD_INPAINT_FLUX2_KLEIN,
    METHOD_INPAINT_FLUX2_KLEIN_ESTIMATE,
    METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_BUILD,
    METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_EXPORT,
    METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_IMPORT,
    METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_LIST,
    METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_LOAD,
    METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_SAVE,
    METHOD_INPAINT_FLUX2_KLEIN_STATUS,
    METHOD_INPAINT_FLUX2_KLEIN_UNLOAD,
)
from modules.ai_backend.ipc.registry import HandlerContext, Interrupted, get_handler

REGION_PNG = b"\x89PNG-region-bytes"
MASK_PNG = b"MASK-bytes!!"
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
        self.status_calls: list[Any] = []
        self.estimate_calls: list[dict[str, Any]] = []
        #: (method, params, extra kwargs) per prompt-cache call.
        self.prompt_cache_calls: list[tuple[str, Any, dict[str, Any]]] = []
        self.unload_calls = 0
        self.unload_return = True

    def inpaint_image_bytes(
        self,
        image_bytes: bytes,
        mask_bytes: bytes,
        *,
        params: dict[str, Any],
        progress_callback: Any = None,
    ) -> dict[str, Any]:
        self.calls.append((image_bytes, mask_bytes, params))
        if progress_callback is not None:
            for frame in PROGRESS_SCRIPT:
                progress_callback(*frame)
        if self._raise_exc is not None:
            raise self._raise_exc
        return self._result

    def status(self, params: dict[str, Any] | None = None) -> dict[str, Any]:
        self.status_calls.append(params)
        return {"available": False, "reason": "Не выбран трансформер"}

    def estimate(
        self, *, params: dict[str, Any], region_width: int, region_height: int
    ) -> dict[str, Any]:
        self.estimate_calls.append(
            {"params": params, "region_width": region_width, "region_height": region_height}
        )
        return {"vram_bytes": 1024, "fits": True}

    def unload(self) -> bool:
        self.unload_calls += 1
        return self.unload_return

    # ---- prompt-cache library ----
    def prompt_cache_build(
        self, params: dict[str, Any] | None, *, progress_callback: Any = None
    ) -> dict[str, Any]:
        self.prompt_cache_calls.append(("build", params, {}))
        if progress_callback is not None:
            for frame in BUILD_PROGRESS_SCRIPT:
                progress_callback(*frame)
        if self._raise_exc is not None:
            raise self._raise_exc
        return {"prompt": "clean", "encoded": True, "prompt_cached": True, "device": "cuda:0"}

    def prompt_cache_list(self, params: dict[str, Any] | None) -> dict[str, Any]:
        self.prompt_cache_calls.append(("list", params, {}))
        return {"family": "text_encoder-abcd1234", "directory": "/x", "entries": [], "skipped": []}

    def prompt_cache_save(
        self, params: dict[str, Any] | None, name: str, *, overwrite: bool = False
    ) -> dict[str, Any]:
        self.prompt_cache_calls.append(("save", params, {"name": name, "overwrite": overwrite}))
        return {"family": "f", "name": name, "path": "/x/f/n.msprompt", "size_bytes": 42}

    def prompt_cache_load(self, params: dict[str, Any] | None, name: str) -> dict[str, Any]:
        self.prompt_cache_calls.append(("load", params, {"name": name}))
        return {"family": "f", "name": name, "prompt": "clean", "prompt_cached": True}

    def prompt_cache_export(
        self, params: dict[str, Any] | None, name: str, path: str
    ) -> dict[str, Any]:
        self.prompt_cache_calls.append(("export", params, {"name": name, "path": path}))
        return {"family": "f", "name": name, "path": path, "size_bytes": 42}

    def prompt_cache_import(
        self,
        params: dict[str, Any] | None,
        path: str,
        *,
        name: str | None = None,
        overwrite: bool = False,
    ) -> dict[str, Any]:
        self.prompt_cache_calls.append(
            ("import", params, {"path": path, "name": name, "overwrite": overwrite})
        )
        return {
            "family": "other-ffffffff",
            "name": name or "theirs",
            "path": "/x/other/theirs.msprompt",
            "size_bytes": 42,
            "family_matches": False,
            "current_family": "text_encoder-abcd1234",
        }


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
        "placement": "encoder_cpu",
        "oom_recovered": False,
        "applied": {
            "unload_transformer_before_vae": True,
            "vae_tiling": True,
            "vae_slicing": True,
            "unload_text_encoder_after_encode": False,
            "text_encoder_fp8": False,
        },
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
        "applied": {
            "unload_transformer_before_vae": True,
            "vae_tiling": True,
            "vae_slicing": True,
            "unload_text_encoder_after_encode": False,
            "text_encoder_fp8": False,
        },
    }
    assert "image_png" not in resp_header


def test_oom_recovery_is_reported_in_the_response_header() -> None:
    result = _default_result()
    result["oom_recovered"] = True
    result["applied"] = {
        "unload_transformer_before_vae": True,
        "vae_tiling": True,
        "vae_slicing": True,
        "unload_text_encoder_after_encode": False,
        "text_encoder_fp8": False,
    }
    svc = _FakeFlux2KleinService(result)
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN)
    resp_header, _blob = handler(
        _ctx(svc), _header({"params": {}}), REGION_PNG + MASK_PNG, _no_cancel()
    )
    assert resp_header["oom_recovered"] is True
    assert resp_header["applied"]["unload_transformer_before_vae"] is True


def test_every_applied_flag_is_forwarded_even_when_the_service_omits_one() -> None:
    # The Rust client parses `applied` as one struct and ignores an incomplete
    # object outright, so a missing key does not degrade the answer — it throws
    # away the OOM-recovery settings the next run was meant to start from.
    result = _default_result()
    result["applied"] = {"vae_tiling": True}
    svc = _FakeFlux2KleinService(result)
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN)
    resp_header, _blob = handler(
        _ctx(svc), _header({"params": {}}), REGION_PNG + MASK_PNG, _no_cancel()
    )
    assert resp_header["applied"] == {
        "unload_transformer_before_vae": False,
        "vae_tiling": True,
        "vae_slicing": False,
        "unload_text_encoder_after_encode": False,
        "text_encoder_fp8": False,
    }


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
# status / estimate / unload
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


def test_estimate_forwards_the_region_size() -> None:
    svc = _FakeFlux2KleinService()
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN_ESTIMATE)
    header, blob = handler(
        _ctx(svc),
        {"params": {"placement": "full_gpu"}, "region_width": 512, "region_height": 256},
        b"",
        _no_cancel(),
    )
    assert blob == b""
    assert header == {"vram_bytes": 1024, "fits": True}
    assert svc.estimate_calls == [
        {"params": {"placement": "full_gpu"}, "region_width": 512, "region_height": 256}
    ]


@pytest.mark.parametrize(
    "header",
    [
        {"params": {}},  # both sizes missing
        {"params": {}, "region_width": 512},
        {"params": {}, "region_width": 0, "region_height": 256},
        {"params": {}, "region_width": "512", "region_height": 256},
    ],
)
def test_estimate_requires_a_positive_region_size(header: dict[str, Any]) -> None:
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN_ESTIMATE)
    with pytest.raises(ValueError):
        handler(_ctx(_FakeFlux2KleinService()), header, b"", _no_cancel())


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
# Prompt-cache library
# ---------------------------------------------------------------------------
def test_prompt_cache_build_streams_the_prompt_phase() -> None:
    svc = _FakeFlux2KleinService()
    emitter, disp = _emitter()
    params = {"prompt": "clean background"}

    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_BUILD)
    header, blob = handler(_ctx(svc, emitter=emitter), {"params": params}, b"", _no_cancel())

    assert [(f[0]["phase"], f[0]["step"]) for f in disp.frames] == [
        (phase, step) for phase, step, _total, _label in BUILD_PROGRESS_SCRIPT
    ]
    assert all(frame_blob == b"" for _h, frame_blob in disp.frames)
    assert svc.prompt_cache_calls == [("build", params, {})]
    assert header["prompt_cached"] is True
    assert blob == b""


def test_prompt_cache_build_without_an_emitter_still_answers() -> None:
    svc = _FakeFlux2KleinService()
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_BUILD)
    header, _blob = handler(_ctx(svc), {"params": None}, b"", _no_cancel())
    assert header["encoded"] is True
    assert svc.prompt_cache_calls == [("build", {}, {})]


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
    header, blob = handler(
        _ctx(svc), {"params": {"text_encoder_path": "/enc"}}, b"", _no_cancel()
    )
    assert header["family"] == "text_encoder-abcd1234"
    assert header["entries"] == []
    assert blob == b""
    assert svc.prompt_cache_calls == [("list", {"text_encoder_path": "/enc"}, {})]


def test_prompt_cache_save_forwards_the_name_and_the_overwrite_flag() -> None:
    svc = _FakeFlux2KleinService()
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_SAVE)
    header, _blob = handler(
        _ctx(svc), {"params": {}, "name": "пресет", "overwrite": True}, b"", _no_cancel()
    )
    assert header["name"] == "пресет"
    assert svc.prompt_cache_calls == [("save", {}, {"name": "пресет", "overwrite": True})]


def test_prompt_cache_save_defaults_to_refusing_an_overwrite() -> None:
    svc = _FakeFlux2KleinService()
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_SAVE)
    handler(_ctx(svc), {"params": {}, "name": "пресет"}, b"", _no_cancel())
    assert svc.prompt_cache_calls[0][2]["overwrite"] is False


@pytest.mark.parametrize(
    "method",
    [
        METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_SAVE,
        METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_LOAD,
    ],
)
@pytest.mark.parametrize("name", [None, "", "   ", 7])
def test_a_missing_or_empty_name_is_a_request_error(method: str, name: Any) -> None:
    header: dict[str, Any] = {"params": {}}
    if name is not None:
        header["name"] = name
    handler = get_handler(method)
    with pytest.raises(ValueError):
        handler(_ctx(_FakeFlux2KleinService()), header, b"", _no_cancel())


def test_prompt_cache_load_returns_the_prompt_from_the_file() -> None:
    svc = _FakeFlux2KleinService()
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_LOAD)
    header, _blob = handler(_ctx(svc), {"params": {}, "name": "пресет"}, b"", _no_cancel())
    assert header["prompt"] == "clean"
    assert header["prompt_cached"] is True


def test_prompt_cache_export_requires_both_a_name_and_a_path() -> None:
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_EXPORT)
    with pytest.raises(ValueError):
        handler(_ctx(_FakeFlux2KleinService()), {"params": {}, "name": "n"}, b"", _no_cancel())
    with pytest.raises(ValueError):
        handler(
            _ctx(_FakeFlux2KleinService()), {"params": {}, "path": "/x.msprompt"}, b"", _no_cancel()
        )

    svc = _FakeFlux2KleinService()
    header, _blob = handler(
        _ctx(svc), {"params": {}, "name": "n", "path": "/x.msprompt"}, b"", _no_cancel()
    )
    assert header["path"] == "/x.msprompt"
    assert svc.prompt_cache_calls == [("export", {}, {"name": "n", "path": "/x.msprompt"})]


def test_prompt_cache_import_reports_a_foreign_family() -> None:
    svc = _FakeFlux2KleinService()
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_IMPORT)
    header, _blob = handler(
        _ctx(svc), {"params": {}, "path": "/in/theirs.msprompt"}, b"", _no_cancel()
    )
    # The Rust side warns on this; the import itself is not an error.
    assert header["family_matches"] is False
    assert header["family"] == "other-ffffffff"
    assert svc.prompt_cache_calls == [
        ("import", {}, {"path": "/in/theirs.msprompt", "name": None, "overwrite": False})
    ]


def test_prompt_cache_import_accepts_an_explicit_name() -> None:
    svc = _FakeFlux2KleinService()
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_IMPORT)
    handler(
        _ctx(svc),
        {"params": {}, "path": "/in/theirs.msprompt", "name": "  взято  ", "overwrite": True},
        b"",
        _no_cancel(),
    )
    assert svc.prompt_cache_calls[0][2] == {
        "path": "/in/theirs.msprompt",
        "name": "взято",
        "overwrite": True,
    }


@pytest.mark.parametrize("bad_name", [7, [], {}])
def test_prompt_cache_import_refuses_a_non_string_name(bad_name: Any) -> None:
    handler = get_handler(METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_IMPORT)
    with pytest.raises(ValueError):
        handler(
            _ctx(_FakeFlux2KleinService()),
            {"params": {}, "path": "/in/x.msprompt", "name": bad_name},
            b"",
            _no_cancel(),
        )


def test_every_prompt_cache_method_is_registered() -> None:
    for method in (
        METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_BUILD,
        METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_LIST,
        METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_SAVE,
        METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_LOAD,
        METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_EXPORT,
        METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_IMPORT,
    ):
        assert get_handler(method) is not None
