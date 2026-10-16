"""
File: modules/ai_backend/inpaint/test_lama_mpe.py

Purpose:
Unit tests for the LaMa-MPE checkpoint gate (`_ensure_checkpoint_locked`).

Main responsibilities:
- verify a missing checkpoint is a `FileNotFoundError` naming the Rust owner of
  the download (`ensure_lama_mpe`) and that no request is ever attempted;
- verify a checkpoint whose SHA-256 differs from the pin is a `RuntimeError`
  and is left in place (the backend never repairs or refetches it);
- verify a checkpoint matching the pin is accepted.

Notes:
The checkpoint path points into a temporary directory and the pin is replaced
by the digest of a small synthetic payload. `urllib.request.urlopen` and
`requests.get` are patched to fail loudly, so a regression that reintroduced a
backend fetch would fail the suite instead of touching the network. No torch.
"""

from __future__ import annotations

import hashlib
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from modules.ai_backend.inpaint import lama_mpe
from modules.ai_backend.runtime.model_manager import SmallModelManager

#: The pin as shipped, captured before any test replaces it.
_SHIPPED_PIN = lama_mpe._LAMA_MPE_SHA256


def _no_network(*_args: object, **_kwargs: object) -> None:
    raise AssertionError("the LaMa-MPE service must never download its checkpoint")


class LamaMpeCheckpointGateTests(unittest.TestCase):
    PAYLOAD = b"lama-mpe checkpoint bytes"

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.service = lama_mpe.LamaMpeInpaintService(SmallModelManager(max_loaded_models=1))
        self.path = Path(tmp.name) / "LaMa_MPE" / "inpainting_lama_mpe.ckpt"
        self.service._model_path = self.path
        import requests
        import urllib.request

        for target, name, value in (
            (lama_mpe, "_LAMA_MPE_SHA256", hashlib.sha256(self.PAYLOAD).hexdigest()),
            (urllib.request, "urlopen", _no_network),
            (requests, "get", _no_network),
        ):
            attr_patch = patch.object(target, name, value)
            attr_patch.start()
            self.addCleanup(attr_patch.stop)

    def _write(self, payload: bytes) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.path.write_bytes(payload)

    def test_a_missing_checkpoint_names_the_rust_owner(self) -> None:
        with self.assertRaises(FileNotFoundError) as caught:
            self.service._ensure_checkpoint_locked()

        message = str(caught.exception)
        self.assertIn("ensure_lama_mpe", message)
        self.assertIn(str(self.path), message)
        self.assertFalse(self.path.exists())

    def test_a_digest_mismatch_is_fatal_and_leaves_the_file(self) -> None:
        self._write(b"x" * len(self.PAYLOAD))

        with self.assertRaises(RuntimeError) as caught:
            self.service._ensure_checkpoint_locked()

        self.assertIn("SHA-256", str(caught.exception))
        self.assertTrue(self.path.exists())

    def test_a_matching_checkpoint_is_accepted(self) -> None:
        self._write(self.PAYLOAD)

        self.assertEqual(self.service._ensure_checkpoint_locked(), self.path)

    def test_the_real_pin_is_the_published_digest(self) -> None:
        # Guard against an accidental edit of the pin; the value equals the
        # bytes the Rust catalog serves (`Torch/LaMa_MPE/inpainting_lama_mpe.ckpt`).
        self.assertEqual(
            _SHIPPED_PIN,
            "d625aa1b3e0d0408acfd6928aa84f005867aa8dbb9162480346a4e20660786cc",
        )


if __name__ == "__main__":
    unittest.main()
