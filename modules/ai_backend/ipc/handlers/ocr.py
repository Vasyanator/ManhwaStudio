"""
File: modules/ai_backend/ipc/handlers/ocr.py

Methods hosted here:
    ocr.manga        — Manga-OCR recognition (METHOD_OCR_MANGA)  [implemented]
    ocr.easy         — EasyOCR recognition (METHOD_OCR_EASY)
    ocr.paddle       — PaddleOCR recognition (METHOD_OCR_PADDLE)
    ocr.paddle_vl    — PaddleOCR-VL recognition (METHOD_OCR_PADDLE_VL)
    ocr.surya        — Surya OCR recognition (METHOD_OCR_SURYA)
    ocr.paddle_onnx  — PaddleOCR-ONNX recognition (METHOD_OCR_PADDLE_ONNX)

Registration pattern — add a new OCR method handler here like this:

    from ..registry import register
    from ..protocol import METHOD_OCR_EASY

    def _handle_ocr_easy(ctx, header, blob, cancel_event):
        ...
        return {"engine": "easyocr", "lines": [...], "text": "..."}, b""

    register(METHOD_OCR_EASY, _handle_ocr_easy)

The ``register`` function is both a plain callable and a decorator:

    @register(METHOD_OCR_EASY)         # decorator form
    def _handle_ocr_easy(...): ...
"""

from __future__ import annotations

import threading
from typing import Any

from ..protocol import (
    METHOD_OCR_EASY,
    METHOD_OCR_MANGA,
    METHOD_OCR_PADDLE,
    METHOD_OCR_PADDLE_ONNX,
    METHOD_OCR_PADDLE_VL,
    METHOD_OCR_SURYA,
)
from ..registry import HandlerContext, Interrupted, register


def _decode_optional_positive_int(header: dict[str, Any], field: str) -> int | None:
    """Mirror of ``server._decode_optional_positive_int``.

    Returns the int value when present and a strictly positive integer, ``None``
    when the field is absent/``None``, and raises ``ValueError`` (surfaced as a
    ``response{status:"error"}`` by the dispatcher) for any invalid value.
    """
    raw = header.get(field)
    if raw is None:
        return None
    if isinstance(raw, bool) or not isinstance(raw, int):
        raise ValueError(f"Field '{field}' must be a positive integer.")
    if raw <= 0:
        raise ValueError(f"Field '{field}' must be a positive integer.")
    return raw


def _handle_ocr_manga(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`ocr.manga`: recognize the request-blob image via Manga-OCR.

    The input image PNG bytes arrive in the request ``blob`` (no base64).
    Request fields (``join_newlines``, ``reflect_strings``, ``manga_model``)
    are inline in the header.  Honors ``cancel_event`` before starting the
    (synchronous) recognition.  Returns ``engine``/``lines``/``text`` inline;
    no response blob.
    """
    if not blob:
        raise ValueError("ocr.manga requires the input image in the frame blob.")
    if cancel_event.is_set():
        raise Interrupted("ocr.manga canceled before start.")

    result = ctx.state.manga_ocr.recognize_image_bytes(
        blob,
        join_newlines=bool(header.get("join_newlines", True)),
        reflect_strings=bool(header.get("reflect_strings", False)),
        manga_model=header.get("manga_model"),
    )

    if cancel_event.is_set():
        raise Interrupted("ocr.manga canceled.")

    return (
        {
            "engine": "mangaocr",
            "lines": result["lines"],
            "text": result["text"],
        },
        b"",
    )


register(METHOD_OCR_MANGA, _handle_ocr_manga)


def _handle_ocr_easy(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`ocr.easy`: recognize the request-blob image via EasyOCR.

    Request fields: ``join_newlines`` (default true), ``reflect_strings``
    (default false), ``easy_langs`` (default ``"ko"``).  Returns
    ``engine``/``lines``/``text`` inline; no response blob.
    """
    if not blob:
        raise ValueError("ocr.easy requires the input image in the frame blob.")
    if cancel_event.is_set():
        raise Interrupted("ocr.easy canceled before start.")

    easy_langs_raw = header.get("easy_langs", "ko")
    if easy_langs_raw is None:
        easy_langs_raw = "ko"
    if not isinstance(easy_langs_raw, str):
        raise ValueError("Field 'easy_langs' must be a string.")
    easy_langs = easy_langs_raw.strip() or "ko"

    result = ctx.state.easy_ocr.recognize_image_bytes(
        blob,
        join_newlines=bool(header.get("join_newlines", True)),
        reflect_strings=bool(header.get("reflect_strings", False)),
        langs=easy_langs,
    )

    if cancel_event.is_set():
        raise Interrupted("ocr.easy canceled.")

    return (
        {
            "engine": "easyocr",
            "lines": result["lines"],
            "text": result["text"],
        },
        b"",
    )


register(METHOD_OCR_EASY, _handle_ocr_easy)


def _handle_ocr_paddle(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`ocr.paddle`: recognize the request-blob image via PaddleOCR.

    Request fields: ``join_newlines`` (default true), ``reflect_strings``
    (default false), ``paddle_lang`` (default ``"korean_v5"``).  Returns
    ``engine``/``lines``/``text`` inline; no response blob.
    """
    if not blob:
        raise ValueError("ocr.paddle requires the input image in the frame blob.")
    if cancel_event.is_set():
        raise Interrupted("ocr.paddle canceled before start.")

    paddle_lang_raw = header.get("paddle_lang", "korean_v5")
    if paddle_lang_raw is None:
        paddle_lang_raw = "korean_v5"
    if not isinstance(paddle_lang_raw, str):
        raise ValueError("Field 'paddle_lang' must be a string.")
    paddle_lang = paddle_lang_raw.strip() or "korean_v5"

    result = ctx.state.paddle_ocr.recognize_image_bytes(
        blob,
        join_newlines=bool(header.get("join_newlines", True)),
        reflect_strings=bool(header.get("reflect_strings", False)),
        lang=paddle_lang,
    )

    if cancel_event.is_set():
        raise Interrupted("ocr.paddle canceled.")

    return (
        {
            "engine": "paddleocr",
            "lines": result["lines"],
            "text": result["text"],
        },
        b"",
    )


register(METHOD_OCR_PADDLE, _handle_ocr_paddle)


def _handle_ocr_paddle_vl(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`ocr.paddle_vl`: recognize the request-blob image via PaddleOCR-VL.

    Request fields: ``join_newlines`` (default true), ``reflect_strings``
    (default false), ``paddle_vl_script`` (optional string, lowercased; empty
    -> ``None``).  Returns ``engine``/``lines``/``text`` inline; no response
    blob.
    """
    if not blob:
        raise ValueError("ocr.paddle_vl requires the input image in the frame blob.")
    if cancel_event.is_set():
        raise Interrupted("ocr.paddle_vl canceled before start.")

    script_raw = header.get("paddle_vl_script")
    if script_raw is not None and not isinstance(script_raw, str):
        raise ValueError("Field 'paddle_vl_script' must be a string.")
    script = str(script_raw or "").strip().lower() or None

    result = ctx.state.paddle_vl_ocr.recognize_image_bytes(
        blob,
        join_newlines=bool(header.get("join_newlines", True)),
        reflect_strings=bool(header.get("reflect_strings", False)),
        script=script,
    )

    if cancel_event.is_set():
        raise Interrupted("ocr.paddle_vl canceled.")

    return (
        {
            "engine": "paddleocrvl",
            "lines": result["lines"],
            "text": result["text"],
        },
        b"",
    )


register(METHOD_OCR_PADDLE_VL, _handle_ocr_paddle_vl)


def _handle_ocr_surya(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`ocr.surya`: recognize the request-blob image via Surya OCR.

    Request fields: ``join_newlines`` (default true), ``reflect_strings``
    (default false), ``surya_task_name`` (default ``"ocr_without_boxes"``,
    lowercased), ``surya_recognize_math``/``surya_sort_lines``/
    ``surya_drop_repeated_text`` (bool, default false), and the optional
    positive ints ``surya_max_sliding_window``/``surya_max_tokens``.  Returns
    ``engine``/``task_name``/``lines``/``text`` inline; no response blob.
    """
    if not blob:
        raise ValueError("ocr.surya requires the input image in the frame blob.")
    if cancel_event.is_set():
        raise Interrupted("ocr.surya canceled before start.")

    task_name_raw = header.get("surya_task_name", "ocr_without_boxes")
    if task_name_raw is None:
        task_name_raw = "ocr_without_boxes"
    if not isinstance(task_name_raw, str):
        raise ValueError("Field 'surya_task_name' must be a string.")
    task_name = task_name_raw.strip().lower() or "ocr_without_boxes"

    max_sliding_window = _decode_optional_positive_int(header, "surya_max_sliding_window")
    max_tokens = _decode_optional_positive_int(header, "surya_max_tokens")

    result = ctx.state.surya_ocr.recognize_image_bytes(
        blob,
        join_newlines=bool(header.get("join_newlines", True)),
        reflect_strings=bool(header.get("reflect_strings", False)),
        task_name=task_name,
        recognize_math=bool(header.get("surya_recognize_math", False)),
        sort_lines=bool(header.get("surya_sort_lines", False)),
        drop_repeated_text=bool(header.get("surya_drop_repeated_text", False)),
        max_sliding_window=max_sliding_window,
        max_tokens=max_tokens,
    )

    if cancel_event.is_set():
        raise Interrupted("ocr.surya canceled.")

    return (
        {
            "engine": "suryaocr",
            "task_name": task_name,
            "lines": result["lines"],
            "text": result["text"],
        },
        b"",
    )


register(METHOD_OCR_SURYA, _handle_ocr_surya)


def _handle_ocr_paddle_onnx(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`ocr.paddle_onnx`: recognize the request-blob image via PaddleOCR (ONNX).

    Manager note: dead from the Rust side; routes through ``paddle_ocr`` exactly
    like ``ocr.paddle`` (mirrors the HTTP ``paddle_onnx_worker``).  Request
    fields: ``join_newlines`` (default true), ``reflect_strings`` (default
    false), ``paddle_onnx_model`` (default ``"korean_v5"``, lowercased),
    ``paddle_onnx_device`` (default ``"cpu"``, lowercased).  Returns
    ``engine``/``model``/``device``/``lines``/``text`` inline; no response blob.
    """
    if not blob:
        raise ValueError("ocr.paddle_onnx requires the input image in the frame blob.")
    if cancel_event.is_set():
        raise Interrupted("ocr.paddle_onnx canceled before start.")

    model_raw = header.get("paddle_onnx_model", "korean_v5")
    if model_raw is None:
        model_raw = "korean_v5"
    if not isinstance(model_raw, str):
        raise ValueError("Field 'paddle_onnx_model' must be a string.")
    model_key = model_raw.strip().lower() or "korean_v5"

    device_raw = header.get("paddle_onnx_device")
    if device_raw is not None and not isinstance(device_raw, str):
        raise ValueError("Field 'paddle_onnx_device' must be a string.")
    device = str(device_raw or "").strip().lower() or "cpu"

    result = ctx.state.paddle_ocr.recognize_image_bytes(
        blob,
        join_newlines=bool(header.get("join_newlines", True)),
        reflect_strings=bool(header.get("reflect_strings", False)),
        lang=model_key,
        device=device,
    )

    if cancel_event.is_set():
        raise Interrupted("ocr.paddle_onnx canceled.")

    return (
        {
            "engine": "paddleocr_onnx",
            "model": model_key,
            "device": device,
            "lines": result["lines"],
            "text": result["text"],
        },
        b"",
    )


register(METHOD_OCR_PADDLE_ONNX, _handle_ocr_paddle_onnx)
