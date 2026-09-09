"""
File: modules/ai_backend/runtime/test_rocm_runtime.py

Purpose:
Unit tests for `rocm_runtime.configure_rocm_runtime`: verify it is a no-op for
non-ROCm / absent Torch builds, that it sets the MIOpen immediate-mode defaults
without overriding explicit user values, and that it forces
`expandable_segments:False` into the allocator-config variable Torch will
ACTUALLY read - overriding an explicit user value there, since that one is a
correctness hazard rather than a preference - unless
`MS_ALLOW_EXPANDABLE_SEGMENTS` opts out.

Notes:
- A fake `torch` module is injected into `sys.modules` so the tests do not need
  a real Torch installation and never run GPU code.
- `error_text`'s ROCm flag is process-global; it is saved and restored per test
  the same way the environment keys are.
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
# `modules/ai_backend/runtime` -> parents[1] = `modules`, parents[2] = repo root.
_PROJECT_ROOT = _MODULE_DIR.parents[2]
if str(_PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(_PROJECT_ROOT))

from modules.ai_backend.runtime.error_text import (  # noqa: E402
    configure_error_text,
    rocm_advice_rewrite_enabled,
)

# Every environment key `configure_rocm_runtime` may write or READ. Saved and
# restored around each test: leaking one of them would make a later test see a
# value the function did not set - and for the three allocator variables it would
# also change WHICH of them the function decides Torch reads.
_MIOPEN_ENV_KEYS = (
    "MIOPEN_FIND_MODE",
    "MIOPEN_USER_DB_PATH",
    "MIOPEN_CUSTOM_CACHE_DIR",
    "PYTORCH_CUDA_ALLOC_CONF",
    "PYTORCH_HIP_ALLOC_CONF",
    "PYTORCH_ALLOC_CONF",
    "MS_ALLOW_EXPANDABLE_SEGMENTS",
)


def _make_fake_torch(hip_version):
    """Build a minimal fake `torch` module with version.hip and cudnn flags."""
    torch = types.ModuleType("torch")
    torch.version = types.SimpleNamespace(hip=hip_version, cuda=None)
    cudnn = types.SimpleNamespace(benchmark=True)
    torch.backends = types.SimpleNamespace(cudnn=cudnn)
    return torch


def _load_rocm_runtime():
    module = importlib.import_module("modules.ai_backend.runtime.rocm_runtime")
    return importlib.reload(module)


class _RocmRuntimeCase(unittest.TestCase):
    """Saves and restores every piece of process-global state these tests move."""

    def setUp(self) -> None:
        self._saved_env = {key: os.environ.get(key) for key in _MIOPEN_ENV_KEYS}
        for key in _MIOPEN_ENV_KEYS:
            os.environ.pop(key, None)
        self._saved_torch = sys.modules.get("torch", None)
        self.addCleanup(configure_error_text, rocm_runtime=rocm_advice_rewrite_enabled())

    def _configure_rocm(self, hip_version: str = "7.2.53211") -> types.ModuleType:
        """Run `configure_rocm_runtime` against a fake ROCm torch; return that fake."""
        fake_torch = _make_fake_torch(hip_version=hip_version)
        sys.modules["torch"] = fake_torch
        rocm_runtime = _load_rocm_runtime()
        with tempfile.TemporaryDirectory() as tmp:
            with patch.object(rocm_runtime, "_resolve_cache_root", lambda: Path(tmp)):
                self.assertTrue(rocm_runtime.configure_rocm_runtime())
        return fake_torch

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


class RocmRuntimeTests(_RocmRuntimeCase):
    def test_no_torch_is_noop(self) -> None:
        # A `None` entry in sys.modules makes `import torch` raise ImportError,
        # which the function must swallow and report as a no-op (False).
        sys.modules["torch"] = None
        rocm_runtime = _load_rocm_runtime()
        self.assertFalse(rocm_runtime.configure_rocm_runtime())
        self.assertNotIn("MIOPEN_FIND_MODE", os.environ)
        self.assertNotIn("PYTORCH_HIP_ALLOC_CONF", os.environ)

    def test_cuda_build_is_noop(self) -> None:
        sys.modules["torch"] = _make_fake_torch(hip_version=None)
        rocm_runtime = _load_rocm_runtime()
        self.assertFalse(rocm_runtime.configure_rocm_runtime())
        self.assertNotIn("MIOPEN_FIND_MODE", os.environ)
        # The HIP allocator pin is ROCm-gated too: a CUDA build must not get it.
        self.assertNotIn("PYTORCH_HIP_ALLOC_CONF", os.environ)

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

    def test_rocm_build_pins_expandable_segments_off(self) -> None:
        self._configure_rocm()
        # Nothing was inherited, so the HIP spelling is created and pinned off.
        self.assertEqual(os.environ["PYTORCH_HIP_ALLOC_CONF"], "expandable_segments:False")
        self.assertNotIn("PYTORCH_CUDA_ALLOC_CONF", os.environ)
        self.assertNotIn("PYTORCH_ALLOC_CONF", os.environ)

    def test_rocm_build_reverses_an_inherited_true(self) -> None:
        # DELIBERATE departure from the `setdefault` rule the rest of this module
        # follows: `expandable_segments:True` is a correctness hazard on this ROCm
        # stack (black images at normal timings), not a performance preference, so
        # an explicit value is overridden rather than honoured.
        os.environ["PYTORCH_HIP_ALLOC_CONF"] = "expandable_segments:True"
        with self.assertLogs("modules.ai_backend.runtime.rocm_runtime", "WARNING") as caught:
            self._configure_rocm(hip_version="7.2")
        self.assertEqual(os.environ["PYTORCH_HIP_ALLOC_CONF"], "expandable_segments:False")
        logged = "\n".join(caught.output)
        # The reversal must say WHAT changed and how to opt out of it.
        self.assertIn("PYTORCH_HIP_ALLOC_CONF", logged)
        self.assertIn("MS_ALLOW_EXPANDABLE_SEGMENTS", logged)

    def test_the_variable_torch_actually_reads_is_the_one_written(self) -> None:
        # Torch stops at the FIRST of CUDA/HIP/ALLOC that is present
        # (`c10/core/AllocatorConfig.cpp`), so writing the HIP spelling while the
        # environment carries the CUDA one would be silently inert.
        os.environ["PYTORCH_CUDA_ALLOC_CONF"] = "max_split_size_mb:128"
        self._configure_rocm()
        self.assertEqual(
            os.environ["PYTORCH_CUDA_ALLOC_CONF"],
            "max_split_size_mb:128,expandable_segments:False",
        )
        # The variable Torch would never reach must NOT be created: doing so would
        # look like the setting is applied while it is not.
        self.assertNotIn("PYTORCH_HIP_ALLOC_CONF", os.environ)

    def test_an_empty_inherited_variable_still_wins(self) -> None:
        # `c10::utils::get_env` returns a value whenever `getenv` is non-null, so
        # an EMPTY assignment is "present" and still stops Torch's search.
        os.environ["PYTORCH_ALLOC_CONF"] = ""
        self._configure_rocm()
        self.assertEqual(os.environ["PYTORCH_ALLOC_CONF"], "expandable_segments:False")
        self.assertNotIn("PYTORCH_HIP_ALLOC_CONF", os.environ)

    def test_other_allocator_options_survive_the_reversal(self) -> None:
        os.environ["PYTORCH_HIP_ALLOC_CONF"] = (
            "max_split_size_mb:64,expandable_segments:true,garbage_collection_threshold:0.8"
        )
        self._configure_rocm()
        self.assertEqual(
            os.environ["PYTORCH_HIP_ALLOC_CONF"],
            "max_split_size_mb:64,expandable_segments:False,"
            "garbage_collection_threshold:0.8",
        )

    def test_an_already_off_value_is_left_byte_identical(self) -> None:
        os.environ["PYTORCH_HIP_ALLOC_CONF"] = "expandable_segments:False,max_split_size_mb:64"
        self._configure_rocm()
        self.assertEqual(
            os.environ["PYTORCH_HIP_ALLOC_CONF"],
            "expandable_segments:False,max_split_size_mb:64",
        )

    def test_the_escape_hatch_leaves_the_users_value_as_found(self) -> None:
        # Silently reversing an explicit environment variable is its own
        # undebuggable surprise, so there is a documented way out - still logged.
        os.environ["PYTORCH_HIP_ALLOC_CONF"] = "expandable_segments:True"
        os.environ["MS_ALLOW_EXPANDABLE_SEGMENTS"] = "1"
        with self.assertLogs("modules.ai_backend.runtime.rocm_runtime", "WARNING") as caught:
            self._configure_rocm()
        self.assertEqual(os.environ["PYTORCH_HIP_ALLOC_CONF"], "expandable_segments:True")
        self.assertIn("MS_ALLOW_EXPANDABLE_SEGMENTS", "\n".join(caught.output))

    def test_the_escape_hatch_creates_nothing_when_no_variable_is_set(self) -> None:
        os.environ["MS_ALLOW_EXPANDABLE_SEGMENTS"] = "1"
        self._configure_rocm()
        for key in ("PYTORCH_CUDA_ALLOC_CONF", "PYTORCH_HIP_ALLOC_CONF", "PYTORCH_ALLOC_CONF"):
            self.assertNotIn(key, os.environ)


class ErrorTextGateTests(_RocmRuntimeCase):
    """`configure_rocm_runtime` owns the ROCm flag `error_text` reads."""

    def test_a_rocm_build_turns_the_advice_rewrite_on(self) -> None:
        configure_error_text(rocm_runtime=False)
        self._configure_rocm()
        self.assertTrue(rocm_advice_rewrite_enabled())

    def test_a_cuda_build_turns_the_advice_rewrite_off(self) -> None:
        # Torch's `expandable_segments` advice is CORRECT on NVIDIA, and the
        # replacement note asserts a ROCm-specific claim that is false there.
        configure_error_text(rocm_runtime=True)
        sys.modules["torch"] = _make_fake_torch(hip_version=None)
        rocm_runtime = _load_rocm_runtime()
        self.assertFalse(rocm_runtime.configure_rocm_runtime())
        self.assertFalse(rocm_advice_rewrite_enabled())

    def test_absent_torch_turns_the_advice_rewrite_off(self) -> None:
        configure_error_text(rocm_runtime=True)
        sys.modules["torch"] = None
        rocm_runtime = _load_rocm_runtime()
        self.assertFalse(rocm_runtime.configure_rocm_runtime())
        self.assertFalse(rocm_advice_rewrite_enabled())


if __name__ == "__main__":
    unittest.main()
