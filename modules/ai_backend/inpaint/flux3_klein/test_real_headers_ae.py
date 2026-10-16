"""
File: modules/ai_backend/inpaint/flux3_klein/test_real_headers_ae.py

Purpose:
Opt-in check that the real `ae.safetensors` of a FLUX.3 klein package matches
`AutoEncoder()` key for key and shape for shape, and carries only the dtypes
`load_autoencoder` expects (F32, plus I64 for the BatchNorm counter).

Notes:
Runs only when `MS_FLUX3_DIR` names the package root; otherwise skipped. Reads
the safetensors HEADER only (no tensor bytes) and builds the model on `meta`,
so it needs no GPU and almost no memory.
"""

from __future__ import annotations

import importlib.util
import os
import unittest
from pathlib import Path

if importlib.util.find_spec("torch") is None:
    raise unittest.SkipTest("torch не установлен")

import torch

from ...runtime.safetensors_stream import read_safetensors_header
from . import autoencoder as ae

_ENV = "MS_FLUX3_DIR"


class RealAeHeaderTests(unittest.TestCase):
    def setUp(self) -> None:
        root = os.environ.get(_ENV)
        if not root:
            self.skipTest(f"{_ENV} не задан")
        self.path = Path(root) / "ae.safetensors"
        if not self.path.is_file():
            self.skipTest(f"нет файла {self.path}")

    def test_keys_shapes_and_dtypes_match_the_model(self) -> None:
        header = read_safetensors_header(self.path)
        header.pop("__metadata__", None)
        with torch.device("meta"):
            model = ae.AutoEncoder()
        expected = {key: list(value.shape) for key, value in model.state_dict().items()}
        self.assertEqual(sorted(set(header) - set(expected)), [])
        self.assertEqual(sorted(set(expected) - set(header)), [])
        for key, shape in expected.items():
            with self.subTest(key=key):
                self.assertEqual(header[key]["shape"], shape)
                wanted = "I64" if key == "bn.num_batches_tracked" else "F32"
                self.assertEqual(header[key]["dtype"], wanted)


if __name__ == "__main__":
    unittest.main()
