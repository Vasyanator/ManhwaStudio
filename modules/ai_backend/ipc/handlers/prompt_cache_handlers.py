"""
File: modules/ai_backend/ipc/handlers/prompt_cache_handlers.py

Purpose:
The bodies of the four `<model>.prompt_cache.*` methods (`build`, `list`,
`export`, `import`), shared by every handler group whose model keeps a prompt
library (FLUX.2 klein and FLUX.3 klein). Registers NO method: a group registers its own
method names and calls these with its service and its method name, so the
request reading, the cancellation contract and the error texts exist once.

Key functions:
- `handle_build()` — streaming; `name` optional, progress through `streaming.progress_forwarder`;
- `handle_list()`, `handle_export()`, `handle_import()` — plain request/response;
- `require_non_empty_str()`, `optional_str()` — the header-field readers these
  methods (and FLUX.2's `component_action`) use.

Notes:
- The service is duck-typed: `prompt_cache_build(params, *, name, progress_callback)`,
  `prompt_cache_list(params)`, `prompt_cache_export(params, file, path)`,
  `prompt_cache_import(params, path)`, each returning the answer mapping.
- Field VALUES are never validated here: what makes a path, an entry file name
  or a preset name acceptable is the service's single answer
  (`prompt_library.require_entry_source` / `require_entry_file_name` /
  `normalize_preset_name`); this layer only checks that a field arrived with
  the right type.
- Not imported by `__init__.py` (it is not a group).
"""

from __future__ import annotations

import threading
import traceback
from typing import Any

from ..registry import HandlerContext, Interrupted
from .streaming import progress_forwarder, require_params


def require_non_empty_str(header: dict[str, Any], field: str) -> str:
    """Read a required non-empty string header field.

    The value itself is NOT validated here: a path or an entry file name is
    untrusted input and the service is the single owner of what makes one
    acceptable. This layer only guarantees the field arrived and is a string.

    # Raises
    `ValueError` when the field is absent, not a string, or blank.
    """
    value = header.get(field)
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"Field '{field}' must be a non-empty string.")
    return value


def optional_str(header: dict[str, Any], field: str) -> str | None:
    """An optional string header field: `None` when absent or `null`, else the string (any content; the service judges it).

    # Raises
    `ValueError` when present and not a string.
    """
    value = header.get(field)
    if value is None:
        return None
    if not isinstance(value, str):
        raise ValueError(f"Field '{field}' must be a string when present.")
    return value


def handle_build(ctx: HandlerContext, header: dict[str, Any], cancel_event: threading.Event, *, service: Any, method: str) -> tuple[dict[str, Any], bytes]:
    """`<method>`: save the prompt as a named preset of the model's library; no image is produced.

    `name` is optional (absent / empty: keep the preset's name, or derive one).
    Streams `phase:"load"` frames for `method` while it encodes — reading a
    text encoder takes tens of seconds and a silent wait is not acceptable.

    Cancellation follows the shared inpaint contract: checked before the call
    and after it returns, never inside the encoder read.

    # Raises
    `Interrupted` when `cancel_event` is set; `ValueError` /
    `FileNotFoundError` from the request or the service, unchanged.
    """
    if cancel_event.is_set():
        raise Interrupted(f"{method} canceled before start.")
    params_raw = require_params(header)
    name = optional_str(header, "name")
    try:
        result = service.prompt_cache_build(params_raw, name=name, progress_callback=progress_forwarder(ctx, method))
    except (ValueError, FileNotFoundError):
        raise
    except Exception:  # noqa: BLE001
        if cancel_event.is_set():
            raise Interrupted(f"{method} canceled.") from None
        traceback.print_exc()
        raise
    if cancel_event.is_set():
        raise Interrupted(f"{method} canceled.")
    return dict(result), b""


def handle_list(ctx: HandlerContext, header: dict[str, Any], cancel_event: threading.Event, *, service: Any, method: str) -> tuple[dict[str, Any], bytes]:
    """`<method>`: list the presets of the library named in `params.prompt_library`."""
    params_raw = require_params(header)
    return dict(service.prompt_cache_list(params_raw)), b""


def handle_export(ctx: HandlerContext, header: dict[str, Any], cancel_event: threading.Event, *, service: Any, method: str) -> tuple[dict[str, Any], bytes]:
    """`<method>`: copy the library entry `file` to the `.msprompt` path in `path`.

    # Raises
    `ValueError` when `file` or `path` is missing; whatever the service raises.
    """
    params_raw = require_params(header)
    file = require_non_empty_str(header, "file")
    path = require_non_empty_str(header, "path")
    return dict(service.prompt_cache_export(params_raw, file, path)), b""


def handle_import(ctx: HandlerContext, header: dict[str, Any], cancel_event: threading.Event, *, service: Any, method: str) -> tuple[dict[str, Any], bytes]:
    """`<method>`: copy a `.msprompt` file into the library it belongs to.

    `library_matches` in the answer tells the client whether that library is
    the one the request names; the import never fails over that, because the
    file is filed under ITS OWN library and would otherwise be lost.

    # Raises
    `ValueError` when `path` is missing; whatever the service raises.
    """
    params_raw = require_params(header)
    path = require_non_empty_str(header, "path")
    return dict(service.prompt_cache_import(params_raw, path)), b""
