"""
File: modules/ai_backend/ocr/test_script_constraint.py

Purpose:
Unit tests for the PaddleOCR-VL hard script-constraint logic.

Main responsibilities:
- verify script-key normalization (aliases / auto);
- verify the stateful UTF-8 allowlist keeps target-script bytes (incl. byte
  fallback), digits, and whitespace while banning other scripts;
- verify EOS is only allowed on a complete-character boundary;
- verify incremental pending advancement matches a full walk;
- verify a NON-special `<|LOC_n|>` spotting token (PaddleOCR-VL 1.5/1.6 made
  them ordinary vocabulary entries) is never allowed under any script.

Notes:
A lightweight fake byte index stands in for `TokenByteIndex` so these tests need
no tokenizer or model.
"""

from __future__ import annotations

import unittest

from modules.ai_backend.ocr.script_constraint import (
    ScriptConstraint,
    TokenByteIndex,
    normalize_script,
)


class _FakeIndex:
    """Minimal stand-in for `TokenByteIndex` with explicit per-id byte mappings."""

    def __init__(self, token_bytes: list[bytes], special_ids: set[int], eos_id: int):
        self.token_bytes = token_bytes
        self.special_ids = set(special_ids)
        self.eos_id = eos_id


# id: 0='요'(C694), 1='A', 2='1', 3..5 = byte fallback for '새'(C0C8 = EC 83 88),
# 6 = EOS (special, no text bytes).
_TOKEN_BYTES = [
    "요".encode("utf-8"),
    b"A",
    b"1",
    b"\xec",
    b"\x83",
    b"\x88",
    b"",
]
_EOS = 6


def _korean() -> ScriptConstraint:
    return ScriptConstraint(_FakeIndex(_TOKEN_BYTES, {_EOS}, _EOS), "korean")


class NormalizeScriptTests(unittest.TestCase):
    def test_auto_and_blank_map_to_none(self) -> None:
        for value in ("", "auto", "  AUTO ", "none", None):
            self.assertIsNone(normalize_script(value))

    def test_known_aliases(self) -> None:
        self.assertEqual(normalize_script("ko"), "korean")
        self.assertEqual(normalize_script("Hangul"), "korean")
        self.assertEqual(normalize_script("zh"), "chinese")
        self.assertEqual(normalize_script("jp"), "japanese")

    def test_unknown_is_none(self) -> None:
        self.assertIsNone(normalize_script("klingon"))


class KoreanConstraintTests(unittest.TestCase):
    def test_empty_pending_allows_target_digit_prefix_and_eos(self) -> None:
        allowed = set(_korean()._allowed_ids(b""))
        self.assertIn(0, allowed)  # whole Hangul syllable
        self.assertIn(2, allowed)  # digit
        self.assertIn(3, allowed)  # valid Hangul byte-fallback lead
        self.assertIn(_EOS, allowed)  # stop on a complete boundary
        self.assertNotIn(1, allowed)  # Latin 'A' banned
        self.assertNotIn(4, allowed)  # bare continuation byte cannot lead

    def test_byte_fallback_completes_target_syllable(self) -> None:
        ko = _korean()
        # Pending after first byte of '새' allows the next continuation byte only.
        allowed_1 = set(ko._allowed_ids(b"\xec"))
        self.assertIn(4, allowed_1)
        self.assertNotIn(_EOS, allowed_1)  # cannot stop mid-character
        # Pending after two bytes completes the syllable on the third.
        allowed_2 = set(ko._allowed_ids(b"\xec\x83"))
        self.assertIn(5, allowed_2)

    def test_advance_matches_pending_walk(self) -> None:
        ko = _korean()
        pending = b""
        for tid in (0, 3, 4, 5):  # '요' then byte-fallback '새'
            pending = ko._advance(pending, _TOKEN_BYTES[tid])
        self.assertEqual(pending, b"")  # ends on a complete-character boundary

    def test_prefix_fn_constrains_only_generated_tail(self) -> None:
        import types

        ko = _korean()
        fn = ko.prefix_fn(prompt_len=2)

        def fake_input(ids: list[int]):
            return types.SimpleNamespace(tolist=lambda: ids)

        # First two ids are the prompt and must be ignored.
        allowed = set(fn(0, fake_input([99, 99])))
        self.assertIn(0, allowed)
        self.assertNotIn(1, allowed)


class _FakeTokenizer:
    """Tokenizer surface `TokenByteIndex` reads, shaped like the 1.6 vocab."""

    # 0 '요', 1 '▁1', 2 '<|LOC_0|>' (non-special in 1.5/1.6), 3 '<0xEC>', 4 '</s>' (EOS).
    _TOKENS = ["요", "▁1", "<|LOC_0|>", "<0xEC>", "</s>"]
    all_special_ids = [4]
    eos_token_id = 4

    def __len__(self) -> int:
        return len(self._TOKENS)

    def convert_ids_to_tokens(self, ids: list[int]) -> list[str]:
        return [self._TOKENS[i] for i in ids]


class NonSpecialLocTokenTests(unittest.TestCase):
    def test_loc_token_contributes_its_text_bytes(self) -> None:
        index = TokenByteIndex(_FakeTokenizer())
        self.assertEqual(index.token_bytes[2], b"<|LOC_0|>")
        self.assertEqual(index.token_bytes[4], b"")

    def test_loc_token_is_banned_under_every_script(self) -> None:
        index = TokenByteIndex(_FakeTokenizer())
        for script in ("korean", "chinese", "japanese"):
            with self.subTest(script=script):
                allowed = set(ScriptConstraint(index, script)._allowed_ids(b""))
                # 'L', 'O', 'C' fall outside every script range.
                self.assertNotIn(2, allowed)
                self.assertIn(1, allowed)  # digit with a space prefix stays legal
                self.assertIn(4, allowed)  # EOS on a complete boundary


if __name__ == "__main__":
    unittest.main()
