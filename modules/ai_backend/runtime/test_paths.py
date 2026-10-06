"""
File: modules/ai_backend/runtime/test_paths.py

Purpose:
Characterization tests for the on-disk model roots resolved from
`runtime/paths.py`.

Main responsibilities:
- pin the side-model directory of every backend consumer (FLUX.1-Fill,
  FLUX.2 klein variants, watermark removal, Reline) to
  `program_root()/ManhwaStudio_AI_Models/side_models/<name>`;
- pin the same directories to the root `config.py` constants that still list
  them for folder creation, so the two owners cannot drift apart silently.

Notes:
- Read-only: nothing is created or written. Importing root `config` is what
  the backend itself does at startup (it creates the model folders it lists),
  and the existing reline/watermark suites already import it.
"""

from __future__ import annotations

import os
import unittest
from pathlib import Path

import config
from modules.ai_backend.inpaint import flux2_download, flux_fill
from modules.ai_backend.reline import service as reline_service
from modules.ai_backend.runtime import paths
from modules.ai_backend.watermark import code_fetch


def _expected_side_dir(name: str) -> Path:
    """`program_root()/ManhwaStudio_AI_Models/side_models/<name>`, spelled out literally."""
    return paths.program_root() / "ManhwaStudio_AI_Models" / "side_models" / name


class SideModelsRootTest(unittest.TestCase):
    def test_side_models_root_is_under_program_root(self) -> None:
        self.assertEqual(paths.side_models_root(), paths.program_root() / "ManhwaStudio_AI_Models" / "side_models")
        self.assertEqual(paths.side_models_root(), Path(config.SIDE_MODELS_DIR))

    def test_config_program_dir_matches_program_root(self) -> None:
        self.assertEqual(Path(config.program_dir), paths.program_root())


class ConsumerDirectoriesTest(unittest.TestCase):
    def test_flux_fill_dirs(self) -> None:
        flux_dir = flux_fill._flux_dir()
        components_dir = flux_fill._components_dir()
        # Callers do string path arithmetic (`os.path.dirname`), so the str type is contract.
        self.assertIsInstance(flux_dir, str)
        self.assertIsInstance(components_dir, str)
        self.assertEqual(flux_dir, config.FLUX_FILL_DIR)
        self.assertEqual(components_dir, config.FLUX_FILL_COMPONENTS_DIR)
        self.assertEqual(flux_dir, os.fspath(_expected_side_dir("FLUX.1-Fill-dev-GGUF")))
        self.assertEqual(components_dir, os.fspath(_expected_side_dir("FLUX.1-Fill-dev-GGUF") / "components"))

    def test_flux2_model_root_per_variant(self) -> None:
        self.assertTrue(flux2_download.VARIANTS)
        for variant in flux2_download.VARIANTS.values():
            with self.subTest(variant=variant.key):
                root = flux2_download.model_root(variant)
                self.assertIsInstance(root, Path)
                self.assertEqual(root, Path(config.SIDE_MODELS_DIR) / variant.dir_name)
                self.assertEqual(root, _expected_side_dir(variant.dir_name))

    def test_watermark_dir(self) -> None:
        root = code_fetch._watermark_dir()
        self.assertIsInstance(root, Path)
        self.assertEqual(root, Path(config.WATERMARK_DIR))
        self.assertEqual(root, _expected_side_dir("WatermarkRemoval"))

    def test_reline_model_dir(self) -> None:
        self.assertIsInstance(reline_service.MODEL_DIR, Path)
        self.assertEqual(reline_service.MODEL_DIR, Path(config.MODELS_DIR) / "side_models" / "Reline")
        self.assertEqual(reline_service.MODEL_DIR, _expected_side_dir("Reline"))
        self.assertEqual(reline_service.DOWNLOAD_DIR, _expected_side_dir("Reline") / ".download")


if __name__ == "__main__":
    unittest.main()
