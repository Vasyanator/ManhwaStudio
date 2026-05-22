from __future__ import annotations

import os
import re
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
