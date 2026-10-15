"""
File: modules/new_project/common.py

Purpose:
Helpers shared by the two fetch daemons (`adv_fetch_cli.py`, `adv_fetch_cloak_cli.py`):
page-file sort keys, wildcard URL / file-name patterns, and the one rule for where a
daemon's `_debug_log` / `_warn_log` diagnostics are written.

Key functions:
- sort_key_for_path(), compile_wildcard_prefixes(), compile_wildcard_fullmatch()
- enable_daemon_stdout_log(), write_diagnostic()

Notes:
Diagnostics are written exactly once. Hosted in-process by the AI backend (the default)
they go through the daemon's Python logger only: the backend's stdout and stderr both
reach `last.log`, so an extra stdout copy printed every line twice. The manual
`--daemon` stdio fallback calls `enable_daemon_stdout_log()` once from its `main()`,
and from then on diagnostics are JSON `log` events on stdout (its single log channel).
"""

from __future__ import annotations

import json
import logging
import os
import re
import sys
import threading
from typing import Optional, Tuple

IMG_EXT = {".jpg", ".jpeg", ".png", ".bmp", ".webp", ".tif", ".tiff"}
RES_PAT = re.compile(r"^(resou?rce)(?:\((\d+)\))?$", re.IGNORECASE)


def parse_part(s: str) -> Tuple[int, int]:
    """Return (value_as_int, leading_zeros) for stable tie-breaks."""
    val = int(s)
    stripped = s.lstrip("0")
    zeros = len(s) - (len(stripped) if stripped != "" else 1)
    return val, zeros


def sort_key_for_path(path: str) -> Tuple[int, int, int, int, int]:
    name = os.path.splitext(os.path.basename(path))[0]
    parts = name.split("_", 1)

    x_val, x_zeros = parse_part(parts[0])
    if len(parts) == 1:
        return (x_val, 0, -1, x_zeros, 0)
    y_val, y_zeros = parse_part(parts[1])
    return (x_val, 1, y_val, x_zeros, y_zeros)


def compile_wildcard_prefixes(pat: str) -> Optional[re.Pattern]:
    """
    Convert a wildcard pattern string to a regex that checks URL prefix matches.
    """
    if not pat:
        return None
    parts = [p.strip() for p in re.split(r"[,\s|]+", pat) if p.strip()]
    if not parts:
        return None

    rx_parts = []
    for p in parts:
        esc = re.escape(p)
        esc = esc.replace(r"\*", ".*").replace(r"\?", ".")
        esc = re.sub(r"\\\[([^\\]+)\\\]", r"[\1]", esc)
        rx_parts.append(f"^{esc}")

    try:
        return re.compile("|".join(rx_parts))
    except re.error:
        return None


def compile_wildcard_fullmatch(pat: str) -> Optional[re.Pattern]:
    """
    Convert a wildcard pattern string to a regex for a full filename match.
    """
    if not pat:
        return None
    parts = [p.strip() for p in re.split(r"[,\s|]+", pat) if p.strip()]
    if not parts:
        return None

    rx_parts = []
    for p in parts:
        esc = re.escape(p)
        esc = esc.replace(r"\*", ".*").replace(r"\?", ".")
        esc = re.sub(r"\\\[([^\\]+)\\\]", r"[\1]", esc)
        rx_parts.append(f"^{esc}$")

    try:
        return re.compile("|".join(rx_parts))
    except re.error:
        return None


# Set once by a daemon `main()` before its stdin loop starts; never reset in production.
_daemon_stdout_log = False


def enable_daemon_stdout_log() -> None:
    """Switch diagnostics to stdout JSON `log` events (the `--daemon` fallback); call once at startup."""
    global _daemon_stdout_log
    _daemon_stdout_log = True


def write_diagnostic(
    logger: logging.Logger,
    emit_lock: threading.Lock,
    level: int,
    wire_level: str,
    message: str,
    args: tuple[object, ...],
) -> None:
    """Write one `%`-style diagnostic to the channel of the current host mode, exactly once.

    In-process: `logger.log(level, message, *args)`. In the `--daemon` fallback: one JSON
    `{"event": "log", "level": wire_level, "message": ...}` line on stdout under
    `emit_lock` (the daemon's stdout lock, shared with its other events). A bad format
    string never loses the line: the raw message and its args are written instead.
    """
    if not _daemon_stdout_log:
        logger.log(level, message, *args)
        return
    try:
        formatted = message % args if args else message
    except Exception:  # noqa: BLE001 - never lose a diagnostic to a bad format string
        formatted = f"{message} | args={args!r}"
    with emit_lock:
        sys.stdout.write(json.dumps({"event": "log", "level": wire_level, "message": formatted}, ensure_ascii=False) + "\n")
        sys.stdout.flush()
