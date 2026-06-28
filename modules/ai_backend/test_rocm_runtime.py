"""
File: modules/ai_backend/test_rocm_runtime.py

Purpose:
Unit tests for `rocm_runtime.configure_rocm_runtime`: verify it is a no-op for
non-ROCm / absent Torch builds and that it sets MIOpen immediate-mode defaults
(without overriding explicit user values) for a ROCm/HIP build.

Notes:
- A fake `torch` module is injected into `sys.modules` so the tests do not need
  a real Torch installation and never run GPU code.
"""

from __future__ import annotations

import importlib
import os
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest.mock import patch

_MODULE_DIR = Path(__file__).resolve().parent
_PROJECT_ROOT = _MODULE_DIR.parents[1]
if str(_PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(_PROJECT_ROOT))

_MIOPEN_ENV_KEYS = (
    "MIOPEN_FIND_MODE",
    "MIOPEN_USER_DB_PATH",
    "MIOPEN_CUSTOM_CACHE_DIR",
)


def _make_fake_torch(hip_version):
    """Build a minimal fake `torch` module with version.hip and cudnn flags."""
    torch = types.ModuleType("torch")
    torch.version = types.SimpleNamespace(hip=hip_version, cuda=None)
    cudnn = types.SimpleNamespace(benchmark=True)
    torch.backends = types.SimpleNamespace(cudnn=cudnn)
    return torch


def _load_rocm_runtime():
    module = importlib.import_module("modules.ai_backend.rocm_runtime")
    return importlib.reload(module)


class RocmRuntimeTests(unittest.TestCase):
    def setUp(self) -> None:
        self._saved_env = {key: os.environ.get(key) for key in _MIOPEN_ENV_KEYS}
        for key in _MIOPEN_ENV_KEYS:
            os.environ.pop(key, None)
        self._saved_torch = sys.modules.get("torch", None)

    def tearDown(self) -> None:
        for key, value in self._saved_env.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value
        if self._saved_torch is None:
            sys.modules.pop("torch", None)
        else:
            sys.modules["torch"] = self._saved_torch

    def test_no_torch_is_noop(self) -> None:
        # A `None` entry in sys.modules makes `import torch` raise ImportError,
        # which the function must swallow and report as a no-op (False).
        sys.modules["torch"] = None
        rocm_runtime = _load_rocm_runtime()
        self.assertFalse(rocm_runtime.configure_rocm_runtime())
        self.assertNotIn("MIOPEN_FIND_MODE", os.environ)

    def test_cuda_build_is_noop(self) -> None:
        sys.modules["torch"] = _make_fake_torch(hip_version=None)
        rocm_runtime = _load_rocm_runtime()
        self.assertFalse(rocm_runtime.configure_rocm_runtime())
        self.assertNotIn("MIOPEN_FIND_MODE", os.environ)

    def test_rocm_build_sets_immediate_mode(self) -> None:
        fake_torch = _make_fake_torch(hip_version="7.2.53211")
        sys.modules["torch"] = fake_torch
        rocm_runtime = _load_rocm_runtime()
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            with patch.object(rocm_runtime, "_resolve_cache_root", lambda: tmp_path):
                self.assertTrue(rocm_runtime.configure_rocm_runtime())

            self.assertEqual(os.environ["MIOPEN_FIND_MODE"], "2")
            self.assertTrue(
                os.environ["MIOPEN_USER_DB_PATH"].startswith(str(tmp_path))
            )
            self.assertTrue(
                os.environ["MIOPEN_CUSTOM_CACHE_DIR"].startswith(str(tmp_path))
            )
            self.assertFalse(fake_torch.backends.cudnn.benchmark)
            self.assertTrue((tmp_path / "miopen" / "user_db").is_dir())
            self.assertTrue((tmp_path / "miopen" / "kernels").is_dir())

    def test_rocm_build_keeps_user_override(self) -> None:
        os.environ["MIOPEN_FIND_MODE"] = "1"
        fake_torch = _make_fake_torch(hip_version="7.2")
        sys.modules["torch"] = fake_torch
        rocm_runtime = _load_rocm_runtime()
        with tempfile.TemporaryDirectory() as tmp:
            with patch.object(rocm_runtime, "_resolve_cache_root", lambda: Path(tmp)):
                self.assertTrue(rocm_runtime.configure_rocm_runtime())
            # Explicit user value must win over the FAST default.
            self.assertEqual(os.environ["MIOPEN_FIND_MODE"], "1")


if __name__ == "__main__":
    unittest.main()
