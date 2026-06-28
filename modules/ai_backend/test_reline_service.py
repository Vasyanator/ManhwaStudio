"""
File: modules/ai_backend/test_reline_service.py

Purpose:
Unit tests for Reline model catalog and filename resolution.

Main responsibilities:
- verify compound archive suffixes map to the same model id as bare model names;
- verify direct model URLs keep the downloadable filename extension;
- verify extracted checkpoint files satisfy later catalog-name lookups.
"""

from __future__ import annotations

import tempfile
import tarfile
import unittest
from io import BytesIO
from pathlib import Path
from unittest.mock import patch

from modules.ai_backend import reline_service


class RelineModelResolutionTests(unittest.TestCase):
    def test_catalog_match_accepts_bare_name_for_tar_xz_entry(self) -> None:
        catalog = [
            {
                "filename": "1x-MangaJPEGHQ.tar.xz",
                "name": "1x-MangaJPEGHQ",
                "url": "https://bucket.yor.ovh/torch_models/1x-MangaJPEGHQ.tar.xz",
            }
        ]

        with patch.object(reline_service, "_fetch_catalog", return_value=catalog):
            self.assertIs(reline_service._find_catalog_entry("1x-MangaJPEGHQ"), catalog[0])
            self.assertIs(reline_service._find_catalog_entry("1x-MangaJPEGHQ.tar.xz"), catalog[0])
            self.assertIs(
                reline_service._find_catalog_entry(
                    "https://bucket.yor.ovh/torch_models/1x-MangaJPEGHQ.tar.xz"
                ),
                catalog[0],
            )

    def test_list_models_uses_model_name_without_compound_suffix(self) -> None:
        catalog = [
            {
                "filename": "1x-MangaJPEGHQ.tar.xz",
                "url": "https://bucket.yor.ovh/torch_models/1x-MangaJPEGHQ.tar.xz",
            },
            {
                "filename": "2x_spanplus.pth",
                "url": "https://bucket.yor.ovh/torch_models/2x_spanplus.pth",
            },
        ]
        with (
            patch.object(reline_service, "_fetch_catalog", return_value=catalog),
            patch.object(reline_service, "_find_existing_model", return_value=None),
        ):
            models = reline_service.RelineService().list_models()

        # Catalog models keep their order; built-in EXTRA_MODELS are appended afterwards.
        names = [model["name"] for model in models]
        self.assertEqual(names[:2], ["1x-MangaJPEGHQ", "2x_spanplus"])
        for extra in reline_service.EXTRA_MODELS:
            self.assertIn(extra["name"], names)

    def test_list_models_marks_extra_downloaded_when_local_file_present(self) -> None:
        extra = reline_service.EXTRA_MODELS[0]
        with (
            patch.object(reline_service, "_fetch_catalog", return_value=[]),
            patch.object(
                reline_service,
                "_find_existing_model",
                return_value=Path("/tmp/local.pth"),
            ),
        ):
            models = reline_service.RelineService().list_models()

        match = next(model for model in models if model["name"] == extra["name"])
        self.assertTrue(match["downloaded"])

    def test_resolve_extra_model_without_url_raises_manual_hint(self) -> None:
        extra = reline_service.EXTRA_MODELS[0]
        with patch.object(reline_service, "_find_existing_model", return_value=None):
            with self.assertRaises(FileNotFoundError) as ctx:
                reline_service._resolve_model({"model_name": extra["name"]})

        message = str(ctx.exception)
        self.assertIn(extra["name"], message)
        self.assertIn(extra["source"], message)

    def test_existing_extracted_model_satisfies_archive_catalog_name(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            model_dir = Path(tmp_dir)
            (model_dir / "1x-MangaJPEGHQ.pth").write_bytes(b"checkpoint")

            with patch.object(reline_service, "MODEL_DIR", model_dir):
                self.assertEqual(
                    reline_service._find_existing_model("1x-MangaJPEGHQ.tar.xz"),
                    model_dir / "1x-MangaJPEGHQ.pth",
                )
                self.assertEqual(
                    reline_service._find_existing_model("1x-MangaJPEGHQ"),
                    model_dir / "1x-MangaJPEGHQ.pth",
                )

    def test_archive_extraction_uses_archive_stem_for_stable_cache_name(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            root = Path(tmp_dir)
            model_dir = root / "models"
            model_dir.mkdir()
            archive_path = root / "1x-MangaJPEGHQ.tar.xz"

            payload = b"checkpoint"
            with tarfile.open(archive_path, mode="w:xz") as archive:
                member = tarfile.TarInfo("nested/model.pth")
                member.size = len(payload)
                archive.addfile(member, BytesIO(payload))

            with patch.object(reline_service, "MODEL_DIR", model_dir):
                extracted = reline_service._extract_first_model_from_archive(archive_path)

            self.assertEqual(extracted, model_dir / "1x-MangaJPEGHQ.pth")
            self.assertEqual(extracted.read_bytes(), payload)
            self.assertFalse(archive_path.exists())

    def test_direct_url_uses_url_filename_when_model_name_has_no_suffix(self) -> None:
        with (
            patch.object(reline_service, "_find_existing_model", return_value=None),
            patch.object(reline_service, "_download_model") as download_model,
        ):
            download_model.return_value = Path("/tmp/model.pth")

            result = reline_service._resolve_model(
                {
                    "model_name": "custom-short-name",
                    "model_url": "https://example.test/models/custom-short-name.safetensors",
                }
            )

        self.assertEqual(result, Path("/tmp/model.pth"))
        download_model.assert_called_once_with(
            "https://example.test/models/custom-short-name.safetensors",
            "custom-short-name.safetensors",
        )

    def test_catalog_entry_without_filename_uses_url_filename_for_download(self) -> None:
        catalog = [
            {
                "name": "FutureModel",
                "url": "https://bucket.yor.ovh/torch_models/FutureModel.tar.xz",
            }
        ]
        with (
            patch.object(reline_service, "_fetch_catalog", return_value=catalog),
            patch.object(reline_service, "_find_existing_model", return_value=None),
            patch.object(reline_service, "_download_model") as download_model,
        ):
            download_model.return_value = Path("/tmp/FutureModel.pth")

            result = reline_service._resolve_model({"model_name": "FutureModel"})

        self.assertEqual(result, Path("/tmp/FutureModel.pth"))
        download_model.assert_called_once_with(
            "https://bucket.yor.ovh/torch_models/FutureModel.tar.xz",
            "FutureModel.tar.xz",
        )


if __name__ == "__main__":
    unittest.main()
