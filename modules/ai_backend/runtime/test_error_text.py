"""
File: modules/ai_backend/runtime/test_error_text.py

Purpose:
Unit tests for `error_text.sanitize_torch_error`: verify that Torch's
`expandable_segments:True` out-of-memory advice is removed for both env-var
spellings ON ROCm and left alone everywhere else, that the rest of a realistic
Torch message survives verbatim - including a diagnostic that shares a
period-less sentence with the advice - that the transformation is idempotent,
and that degenerate input never raises.

Notes:
- Pure stdlib; no Torch and no GPU involved.
- The rewrite is ROCm-gated, so every test that expects a rewrite turns it on
  through `configure_error_text` and restores the previous state afterwards.
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

_MODULE_DIR = Path(__file__).resolve().parent
# `modules/ai_backend/runtime` -> parents[1] = `modules`, parents[2] = repo root.
_PROJECT_ROOT = _MODULE_DIR.parents[2]
if str(_PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(_PROJECT_ROOT))

from modules.ai_backend.runtime.error_text import (  # noqa: E402
    configure_error_text,
    rocm_advice_rewrite_enabled,
    sanitize_torch_error,
)

# The real Torch 2.12 ROCm OOM text, assembled from the literals in
# `libc10_hip.so` (the HIP build still emits the CUDA spelling of the variable).
_TORCH_OOM_CUDA_SPELLING = (
    "HIP out of memory. Tried to allocate 2.00 GiB. GPU 0 has a total capacity of "
    "15.98 GiB of which 512.00 MiB is free. Of the allocated memory 14.20 GiB is "
    "allocated by PyTorch, and 96.00 MiB is reserved by PyTorch but unallocated. "
    "If reserved but unallocated memory is large try setting "
    "PYTORCH_CUDA_ALLOC_CONF=expandable_segments:True to avoid fragmentation.  "
    "See documentation for Memory Management  "
    "(https://docs.pytorch.org/docs/stable/notes/cuda.html#optimizing-memory-usage-with-pytorch-cuda-alloc-conf)"
)

_TORCH_OOM_HIP_SPELLING = _TORCH_OOM_CUDA_SPELLING.replace(
    "PYTORCH_CUDA_ALLOC_CONF=", "PYTORCH_HIP_ALLOC_CONF="
)


#: A Torch OOM message whose diagnostic numbers share ONE period-less sentence
#: with the advice. This is the shape that made the unanchored prefix eat the
#: whole message: there is no sentence boundary in front of `try setting` for
#: `[^.\n]*?` to stop at.
_TORCH_OOM_NO_PRECEDING_PERIOD = (
    "CUDA out of memory: tried to allocate 10 GiB; GPU has 2 GiB free; "
    "try setting PYTORCH_CUDA_ALLOC_CONF=expandable_segments:True to avoid fragmentation"
)


class _RocmRewriteCase(unittest.TestCase):
    """Base case that turns the ROCm-gated rewrite ON and restores it afterwards."""

    rocm = True

    def setUp(self) -> None:
        previous = rocm_advice_rewrite_enabled()
        self.addCleanup(configure_error_text, rocm_runtime=previous)
        configure_error_text(rocm_runtime=self.rocm)


class SanitizeTorchErrorTests(_RocmRewriteCase):
    def test_advice_removed_for_cuda_spelling(self) -> None:
        result = sanitize_torch_error(_TORCH_OOM_CUDA_SPELLING)
        self.assertNotIn("expandable_segments:True", result)
        self.assertNotIn("PYTORCH_CUDA_ALLOC_CONF=", result)
        self.assertNotIn("If reserved but unallocated memory is large", result)

    def test_advice_removed_for_hip_spelling(self) -> None:
        result = sanitize_torch_error(_TORCH_OOM_HIP_SPELLING)
        self.assertNotIn("expandable_segments:True", result)
        self.assertNotIn("PYTORCH_HIP_ALLOC_CONF=", result)

    def test_rest_of_message_preserved_verbatim(self) -> None:
        result = sanitize_torch_error(_TORCH_OOM_CUDA_SPELLING)
        head = (
            "HIP out of memory. Tried to allocate 2.00 GiB. GPU 0 has a total "
            "capacity of 15.98 GiB of which 512.00 MiB is free. Of the allocated "
            "memory 14.20 GiB is allocated by PyTorch, and 96.00 MiB is reserved "
            "by PyTorch but unallocated."
        )
        tail = (
            "See documentation for Memory Management  "
            "(https://docs.pytorch.org/docs/stable/notes/cuda.html#optimizing-memory-usage-with-pytorch-cuda-alloc-conf)"
        )
        # Every diagnostic number and the documentation link must survive: this
        # message is what the user forwards when reporting the failure.
        self.assertTrue(result.startswith(head), result)
        self.assertTrue(result.endswith(tail), result)
        # Only the advice clause was dropped, so the result is strictly shorter
        # than the original minus that clause plus the note.
        self.assertIn("expandable_segments", result)

    def test_replacement_note_is_present(self) -> None:
        result = sanitize_torch_error(_TORCH_OOM_HIP_SPELLING)
        self.assertIn("ManhwaStudio", result)

    def test_idempotent(self) -> None:
        once = sanitize_torch_error(_TORCH_OOM_CUDA_SPELLING)
        twice = sanitize_torch_error(once)
        self.assertEqual(once, twice)
        self.assertEqual(sanitize_torch_error(twice), twice)

    def test_unrelated_text_is_byte_identical(self) -> None:
        for text in (
            "Некорректная маска: ожидается 2D/3D массив",
            "RuntimeError: CUDA error: an illegal memory access was encountered",
            "See documentation for Memory Management and PYTORCH_CUDA_ALLOC_CONF",
            # A different Torch message that mentions the token but gives no
            # env-var assignment: it must not be touched.
            "Tensors allocated with expandable_segments:True cannot be shared "
            "between processes.",
            # The safe value must never be mistaken for the harmful advice.
            "backend pinned PYTORCH_HIP_ALLOC_CONF=expandable_segments:False",
        ):
            with self.subTest(text=text):
                self.assertEqual(sanitize_torch_error(text), text)

    def test_only_the_advice_sentence_is_removed(self) -> None:
        text = (
            "First sentence stays. "
            "Try setting PYTORCH_HIP_ALLOC_CONF=expandable_segments:True to avoid "
            "fragmentation. Third sentence stays."
        )
        result = sanitize_torch_error(text)
        self.assertTrue(result.startswith("First sentence stays. "), result)
        self.assertTrue(result.endswith(" Third sentence stays."), result)
        self.assertNotIn("Try setting", result)

    def test_multiline_text_does_not_bleed_across_lines(self) -> None:
        text = (
            "line one\n"
            "try setting PYTORCH_HIP_ALLOC_CONF=expandable_segments:True to avoid "
            "fragmentation.\n"
            "line three"
        )
        result = sanitize_torch_error(text)
        self.assertTrue(result.startswith("line one\n"), result)
        self.assertTrue(result.endswith("\nline three"), result)

    def test_a_diagnostic_sharing_the_advice_sentence_survives(self) -> None:
        # The regression: with no period in front of `try setting`, an unanchored
        # prefix walked back to the start of the message and the user got the
        # replacement note ALONE. Both numbers are what makes the report usable.
        result = sanitize_torch_error(_TORCH_OOM_NO_PRECEDING_PERIOD)
        self.assertIn("tried to allocate 10 GiB", result)
        self.assertIn("GPU has 2 GiB free", result)
        self.assertTrue(result.startswith("CUDA out of memory: "), result)
        self.assertNotIn("try setting", result)
        self.assertNotIn("expandable_segments:True", result)
        self.assertIn("ManhwaStudio", result)

    def test_the_period_less_form_is_idempotent_too(self) -> None:
        once = sanitize_torch_error(_TORCH_OOM_NO_PRECEDING_PERIOD)
        self.assertEqual(sanitize_torch_error(once), once)

    def test_degenerate_input_never_raises(self) -> None:
        class Explosive:
            def __str__(self) -> str:
                raise ValueError("no text for you")

        self.assertEqual(sanitize_torch_error(""), "")
        # Non-string input is coerced rather than rejected.
        self.assertEqual(sanitize_torch_error(None), "None")  # type: ignore[arg-type]
        self.assertEqual(sanitize_torch_error(42), "42")  # type: ignore[arg-type]
        # A value whose __str__ raises must still not propagate an exception.
        self.assertEqual(sanitize_torch_error(Explosive()), "")  # type: ignore[arg-type]


class SanitizeOutsideRocmTests(_RocmRewriteCase):
    """Off ROCm the message is Torch's own, byte for byte.

    Torch's `expandable_segments` advice is CORRECT on NVIDIA/CUDA, and the
    replacement note asserts a ROCm-specific claim that is false there.
    """

    rocm = False

    def test_the_rewrite_is_off_by_default_state(self) -> None:
        self.assertFalse(rocm_advice_rewrite_enabled())

    def test_torch_advice_passes_through_untouched(self) -> None:
        for text in (
            _TORCH_OOM_CUDA_SPELLING,
            _TORCH_OOM_HIP_SPELLING,
            _TORCH_OOM_NO_PRECEDING_PERIOD,
        ):
            with self.subTest(text=text[:40]):
                self.assertEqual(sanitize_torch_error(text), text)

    def test_degenerate_input_still_never_raises(self) -> None:
        class Explosive:
            def __str__(self) -> str:
                raise ValueError("no text for you")

        self.assertEqual(sanitize_torch_error(""), "")
        self.assertEqual(sanitize_torch_error(None), "None")  # type: ignore[arg-type]
        self.assertEqual(sanitize_torch_error(Explosive()), "")  # type: ignore[arg-type]


if __name__ == "__main__":
    unittest.main()
