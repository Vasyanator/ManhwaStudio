"""
FILE OVERVIEW: modules/ai_backend/script_constraint.py
Hard script (writing-system) constraint for PaddleOCR-VL generation.

Purpose:
Force decoding to stay within a chosen writing system (Korean / Chinese /
Japanese) plus whitespace, digits, and common punctuation. Implemented as a
stateful UTF-8 `prefix_allowed_tokens_fn` for `model.generate`.

Why stateful UTF-8 is required:
PaddleOCR-VL uses a SentencePiece tokenizer with byte_fallback. Most CJK
characters are emitted as 1-3 raw byte tokens (`<0xNN>`), which are
script-agnostic. A plain token allowlist therefore cannot express "Hangul only".
We instead reconstruct the decoded UTF-8 byte stream and only allow byte
continuations whose completed codepoints fall inside the allowed Unicode ranges.

Key types:
- `TokenByteIndex`: maps each vocab id to the UTF-8 bytes it contributes to the
  decoded text (built once per tokenizer; reused across scripts).
- `ScriptConstraint`: per-script allow logic with a pending-state cache and a
  `prefix_allowed_tokens_fn` factory.

Notes:
- Whitespace, digits, and a common punctuation set are always allowed so real
  text (line breaks, numbers, basic marks) is not mangled.
- The constraint reshapes output to one script; it cannot fix genuine misreads
  and will suppress legitimately mixed-script content (e.g. Latin SFX).
"""

from __future__ import annotations

import re
from typing import Any, Callable

# Writing-system Unicode ranges per supported script. Ranges are inclusive.
_SCRIPT_RANGES: dict[str, list[tuple[int, int]]] = {
    # Hangul syllables + conjoining/compatibility Jamo.
    "korean": [(0xAC00, 0xD7A3), (0x1100, 0x11FF), (0x3130, 0x318F), (0xA960, 0xA97F)],
    # CJK unified ideographs (+ Ext A, compat) and CJK symbols/punctuation block.
    "chinese": [(0x4E00, 0x9FFF), (0x3400, 0x4DBF), (0xF900, 0xFAFF), (0x3000, 0x303F)],
    # Hiragana, Katakana, kanji, halfwidth katakana, CJK symbols/punctuation.
    "japanese": [
        (0x3040, 0x309F),
        (0x30A0, 0x30FF),
        (0x4E00, 0x9FFF),
        (0x3400, 0x4DBF),
        (0xFF65, 0xFF9F),
        (0x3000, 0x303F),
    ],
}

# Always-allowed single-byte codepoints (ASCII digits handled via range below).
_WHITESPACE = frozenset({0x09, 0x0A, 0x0D, 0x20})
# Common punctuation/marks kept regardless of script (mostly ASCII + fullwidth).
_PUNCTUATION = frozenset(ord(ch) for ch in ".,!?:;~…·-—‐'\"`()[]{}<>/“”‘’「」『』（）【】、。〜！？％")
_DIGIT_RANGE = (0x30, 0x39)
_FULLWIDTH_DIGITS = (0xFF10, 0xFF19)

_BYTE_TOKEN_RE = re.compile(r"^<0x([0-9A-Fa-f]{2})>$")
# SentencePiece word-boundary marker (U+2581) decodes to a leading space.
_SPM_SPACE = "▁"

SUPPORTED_SCRIPTS = ("korean", "chinese", "japanese")


def normalize_script(raw: Any) -> str | None:
    """Map a user script key to a supported script, or None for auto/unset."""
    value = str(raw or "").strip().lower()
    if value in ("", "auto", "none", "off", "any", "multilingual"):
        return None
    aliases = {
        "korean": "korean",
        "ko": "korean",
        "kor": "korean",
        "hangul": "korean",
        "chinese": "chinese",
        "zh": "chinese",
        "ch": "chinese",
        "cn": "chinese",
        "han": "chinese",
        "japanese": "japanese",
        "ja": "japanese",
        "jp": "japanese",
        "jpn": "japanese",
        "kana": "japanese",
    }
    return aliases.get(value)


def _utf8_seq_len(lead: int) -> int | None:
    """Return the total UTF-8 byte length implied by a lead byte, or None if the
    byte cannot start a character (continuation byte or invalid lead)."""
    if lead < 0x80:
        return 1
    if 0xC2 <= lead < 0xE0:
        return 2
    if 0xE0 <= lead < 0xF0:
        return 3
    if 0xF0 <= lead < 0xF5:
        return 4
    return None


class TokenByteIndex:
    """Maps each tokenizer id to the UTF-8 bytes it contributes to decoded text.

    Byte-fallback tokens (`<0xNN>`) map to their single raw byte. Normal pieces
    map to their text with the SentencePiece space marker expanded, encoded as
    UTF-8. Special tokens contribute no text bytes and are tracked separately.
    """

    def __init__(self, tokenizer: Any) -> None:
        size = len(tokenizer)
        tokens = tokenizer.convert_ids_to_tokens(list(range(size)))
        self.special_ids = set(tokenizer.all_special_ids)
        self.eos_id = tokenizer.eos_token_id
        self.token_bytes: list[bytes] = [b""] * size
        for tid, tok in enumerate(tokens):
            if tid in self.special_ids or tok is None:
                self.token_bytes[tid] = b""
                continue
            match = _BYTE_TOKEN_RE.match(tok)
            if match is not None:
                self.token_bytes[tid] = bytes([int(match.group(1), 16)])
            else:
                self.token_bytes[tid] = tok.replace(_SPM_SPACE, " ").encode(
                    "utf-8", "ignore"
                )


class ScriptConstraint:
    """Stateful UTF-8 allowlist for a single writing system.

    `prefix_fn(prompt_len)` returns a `prefix_allowed_tokens_fn` suitable for
    `model.generate`. Allowed-id lists are cached per pending-byte state, so the
    100k-token scan runs only once per distinct UTF-8 continuation state.
    """

    def __init__(self, index: TokenByteIndex, script: str) -> None:
        if script not in _SCRIPT_RANGES:
            raise ValueError(f"Unsupported script: {script!r}")
        self._index = index
        self._ranges = _SCRIPT_RANGES[script]
        self._cache: dict[bytes, list[int]] = {}

    def _codepoint_allowed(self, cp: int) -> bool:
        if cp in _WHITESPACE or cp in _PUNCTUATION:
            return True
        if _DIGIT_RANGE[0] <= cp <= _DIGIT_RANGE[1]:
            return True
        if _FULLWIDTH_DIGITS[0] <= cp <= _FULLWIDTH_DIGITS[1]:
            return True
        return any(lo <= cp <= hi for lo, hi in self._ranges)

    def _prefix_can_complete(self, pending: bytes) -> bool:
        """Whether an incomplete multi-byte prefix can still complete to an allowed
        multi-byte codepoint. Bounds the codepoint by filling missing continuation
        bytes with their min (0x80) and max (0xBF) and checks range intersection."""
        lead = pending[0]
        total = _utf8_seq_len(lead)
        if total is None or len(pending) >= total:
            return False
        missing = total - len(pending)
        try:
            lo = (pending + bytes([0x80]) * missing).decode("utf-8")
            hi = (pending + bytes([0xBF]) * missing).decode("utf-8")
        except UnicodeDecodeError:
            return False
        lo_cp, hi_cp = ord(lo), ord(hi)
        return any(lo_cp <= hi and lo <= hi_cp for lo, hi in self._ranges)

    def _walk(self, buf: bytes) -> tuple[bool, bytes]:
        """Validate a reconstructed byte buffer. Returns (ok, trailing_pending).

        Every complete codepoint must be allowed; trailing incomplete bytes must
        be a viable prefix of an allowed multi-byte codepoint."""
        i, n = 0, len(buf)
        while i < n:
            total = _utf8_seq_len(buf[i])
            if total is None:
                return (False, b"")
            if i + total <= n:
                try:
                    chunk = buf[i : i + total].decode("utf-8")
                except UnicodeDecodeError:
                    return (False, b"")
                if len(chunk) != 1 or not self._codepoint_allowed(ord(chunk)):
                    return (False, b"")
                i += total
            else:
                rest = buf[i:]
                return (self._prefix_can_complete(rest), rest)
        return (True, b"")

    def _allowed_ids(self, pending: bytes) -> list[int]:
        cached = self._cache.get(pending)
        if cached is not None:
            return cached
        index = self._index
        token_bytes = index.token_bytes
        special_ids = index.special_ids
        out: list[int] = []
        for tid, raw in enumerate(token_bytes):
            if tid in special_ids:
                continue
            ok, _ = self._walk(pending + raw)
            if ok:
                out.append(tid)
        if not pending and index.eos_id is not None:
            # End-of-sequence is only valid on a complete-character boundary.
            out.append(index.eos_id)
        if not out:
            # Never strand generation with an empty allowlist; allow EOS to stop.
            if index.eos_id is not None:
                out.append(index.eos_id)
        self._cache[pending] = out
        return out

    def _advance(self, pending: bytes, raw: bytes) -> bytes:
        """Append an already-accepted token's bytes and return the new trailing
        incomplete prefix. Only the tail can be incomplete because every accepted
        step left the stream on a valid prefix boundary."""
        buf = pending + raw
        i, n = 0, len(buf)
        while i < n:
            total = _utf8_seq_len(buf[i])
            if total is None:
                return b""
            if i + total <= n:
                i += total
            else:
                return buf[i:]
        return b""

    def prefix_fn(self, prompt_len: int) -> Callable[[int, Any], list[int]]:
        """Build a `prefix_allowed_tokens_fn` that ignores the first `prompt_len`
        tokens (prompt + image placeholders) and constrains only generated text.

        Pending UTF-8 state is advanced incrementally per beam (keyed by
        `batch_id`), so each step costs O(new tokens) instead of rescanning the
        whole continuation, and allowed-id lists are cached per pending state."""
        token_bytes = self._index.token_bytes
        # batch_id -> (consumed_len, pending_bytes)
        state: dict[int, tuple[int, bytes]] = {}

        def fn(batch_id: int, input_ids: Any) -> list[int]:
            ids = input_ids.tolist()
            cur_len = len(ids)
            seen_len, pending = state.get(batch_id, (prompt_len, b""))
            # A shorter sequence than last time means a fresh beam: recompute.
            if cur_len < seen_len:
                seen_len, pending = prompt_len, b""
            for tid in ids[seen_len:]:
                pending = self._advance(pending, token_bytes[tid])
            state[batch_id] = (cur_len, pending)
            return self._allowed_ids(pending)

        return fn
