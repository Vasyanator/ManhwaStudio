"""
FILE OVERVIEW: modules/ai_backend/ocr/result_format.py
Single owner of the `{"lines", "text"}` OCR result shape for engines that
produce one raw text block per crop.

Key functions:
- `format_recognition_lines()`: raw model text -> trimmed non-empty lines plus
  the joined string, honoring `join_newlines` / `reflect_strings`.

Notes:
- Used by `paddle_vl.py` and `baberu.py`. Stdlib only, so importing it never
  drags an engine's dependencies into another engine.
"""

from __future__ import annotations

from typing import Any


def format_recognition_lines(
    text: str,
    *,
    join_newlines: bool,
    reflect_strings: bool,
) -> dict[str, Any]:
    """Split raw model text into trimmed non-empty lines and a joined string.

    `\\r\\n` counts as one line break. `join_newlines=False` joins lines with
    spaces; `reflect_strings=True` reverses line order for right-to-left manga
    column reading. Returns `{"lines": list[str], "text": str}`.
    """
    lines = [
        line.strip()
        for line in str(text or "").replace("\r\n", "\n").split("\n")
        if line.strip()
    ]
    if reflect_strings:
        lines.reverse()

    output_text = "\n".join(lines) if join_newlines else " ".join(lines)
    return {
        "lines": lines,
        "text": output_text.strip(),
    }
