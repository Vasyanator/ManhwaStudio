"""
File: modules/ai_backend/reline/test_service.py

Purpose:
Unit tests for Reline model catalog and filename resolution.

Main responsibilities:
- verify compound archive suffixes map to the same model id as bare model names;
- verify direct model URLs keep the downloadable filename extension;
- verify extracted checkpoint files satisfy later catalog-name lookups;
- verify constructing the service creates no directory and a missing `MODEL_DIR` means "not
  downloaded";
- verify `_fetch_catalog` rides `fetch_small_body`: retried, cancellable, and a non-array
  response is an explicit error;
- verify `_download_model` rides the shared retrying transfer: a dropped connection is
  retried and resumed with `Range`, an archive is extracted after the download, a corrupt
  archive is removed rather than kept, a permanent status is an explicit error with no retry,
  an unsupported suffix is refused before any request, and a cancel during a retry wait stops
  with the bytes kept.

Notes:
The transport and the clock are injected (`get=`, `sleep=`) and `MODEL_DIR` / `DOWNLOAD_DIR`
point into a temporary directory, so no test touches the network or the user's model tree.
"""

from __future__ import annotations

import tempfile
import tarfile
import unittest
from io import BytesIO
from pathlib import Path
from unittest.mock import patch

from modules.ai_backend.reline import service as reline_service


class _TempModelDirTestCase(unittest.TestCase):
    """Points `MODEL_DIR` / `DOWNLOAD_DIR` into a temporary directory for every test.

    The directories are NOT created up front: the service must create them itself, and only
    when it downloads.
    """

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.model_dir = Path(tmp.name) / "Reline"
        self.download_dir = self.model_dir / ".download"
        for name, value in (("MODEL_DIR", self.model_dir), ("DOWNLOAD_DIR", self.download_dir)):
            attr_patch = patch.object(reline_service, name, value)
            attr_patch.start()
            self.addCleanup(attr_patch.stop)
        self.sleeps: list[float] = []


class RelineModelResolutionTests(_TempModelDirTestCase):
    def test_construction_creates_no_directory(self) -> None:
        reline_service.RelineService()

        self.assertFalse(self.model_dir.exists())
        self.assertIsNone(reline_service._find_existing_model("2x_anything.pth"))

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
            should_cancel=None,
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
            should_cancel=None,
        )


class _FakeResponse:
    """`requests.Response` surface the shared transfer uses."""

    def __init__(
        self,
        body: bytes,
        *,
        status: int = 200,
        headers: dict[str, str] | None = None,
        raise_after: BaseException | None = None,
    ) -> None:
        self._body = body
        self.status_code = status
        self.headers = {"Content-Length": str(len(body))}
        self.headers.update(headers or {})
        self._raise_after = raise_after

    def __enter__(self) -> "_FakeResponse":
        return self

    def __exit__(self, *_exc_info: object) -> None:
        return None

    def raise_for_status(self) -> None:
        import requests

        if self.status_code >= 400:
            raise requests.HTTPError(f"HTTP {self.status_code}")

    def iter_content(self, chunk_size: int = 1 << 20):
        for start in range(0, len(self._body), 64):
            yield self._body[start : start + 64]
        if self._raise_after is not None:
            raise self._raise_after


class _FakeGet:
    """Injected `get` answering from a list, recording each call's headers."""

    def __init__(self, answers: list[_FakeResponse]) -> None:
        self._answers = list(answers)
        self.headers: list[dict[str, str]] = []

    def __call__(self, _url: str, **kwargs: object) -> _FakeResponse:
        self.headers.append(dict(kwargs.get("headers") or {}))
        if not self._answers:
            raise AssertionError("more requests than scripted")
        return self._answers.pop(0)


class RelineDownloadTests(_TempModelDirTestCase):
    URL = "https://bucket.example/torch_models/2x_test.pth"
    PAYLOAD = bytes(range(256)) * 4

    def _download(
        self, get: _FakeGet, url: str = URL, filename: str = "2x_test.pth", **kwargs
    ) -> Path:
        return reline_service._download_model(
            url, filename, get=get, sleep=self.sleeps.append, **kwargs
        )

    def test_a_cancel_during_the_retry_wait_stops_and_keeps_the_bytes(self) -> None:
        import requests

        from modules.ai_backend.engines.model_download import DownloadCancelled

        total = len(self.PAYLOAD)
        get = _FakeGet(
            [
                _FakeResponse(
                    self.PAYLOAD[:100],
                    headers={"Content-Length": str(total)},
                    raise_after=requests.ConnectionError("reset"),
                )
            ]
        )
        cancelled = {"now": False}

        def sleep(seconds: float) -> None:
            self.sleeps.append(seconds)
            cancelled["now"] = True

        with self.assertRaises(DownloadCancelled):
            reline_service._download_model(
                self.URL, "2x_test.pth", should_cancel=lambda: cancelled["now"], get=get, sleep=sleep
            )

        self.assertFalse((self.model_dir / "2x_test.pth").exists())
        self.assertEqual([p.name for p in self.download_dir.iterdir()], ["2x_test.pth.part"])

    def test_a_dropped_connection_is_retried_and_resumed(self) -> None:
        import requests

        half = len(self.PAYLOAD) // 2
        total = len(self.PAYLOAD)
        get = _FakeGet(
            [
                _FakeResponse(
                    self.PAYLOAD[:half],
                    headers={"Content-Length": str(total)},
                    raise_after=requests.ConnectionError("reset"),
                ),
                _FakeResponse(
                    self.PAYLOAD[half:],
                    status=206,
                    headers={"Content-Range": f"bytes {half}-{total - 1}/{total}"},
                ),
            ]
        )

        result = self._download(get)

        self.assertEqual(result, self.model_dir / "2x_test.pth")
        self.assertEqual(result.read_bytes(), self.PAYLOAD)
        self.assertNotIn("Range", get.headers[0])
        self.assertEqual(get.headers[1].get("Range"), f"bytes={half}-")
        self.assertEqual(get.headers[0].get("User-Agent"), reline_service.USER_AGENT)
        self.assertGreater(sum(self.sleeps), 0)
        self.assertEqual(list(self.download_dir.iterdir()), [])

    def test_a_downloaded_archive_is_extracted_under_its_stem(self) -> None:
        payload = b"checkpoint"
        buffer = BytesIO()
        with tarfile.open(fileobj=buffer, mode="w:xz") as archive:
            member = tarfile.TarInfo("nested/model.pth")
            member.size = len(payload)
            archive.addfile(member, BytesIO(payload))

        result = self._download(
            _FakeGet([_FakeResponse(buffer.getvalue())]),
            url="https://bucket.example/torch_models/1x-Manga.tar.xz",
            filename="1x-Manga.tar.xz",
        )

        self.assertEqual(result, self.model_dir / "1x-Manga.pth")
        self.assertEqual(result.read_bytes(), payload)
        self.assertEqual(list(self.download_dir.iterdir()), [])

    def test_a_corrupt_archive_is_removed_so_the_next_request_refetches(self) -> None:
        with self.assertRaises(RuntimeError):
            self._download(
                _FakeGet([_FakeResponse(b"not an xz stream")]),
                url="https://bucket.example/torch_models/1x-Bad.tar.xz",
                filename="1x-Bad.tar.xz",
            )

        self.assertEqual(list(self.download_dir.iterdir()), [])

    def test_a_permanent_status_is_an_explicit_error_without_retry(self) -> None:
        with self.assertRaises(RuntimeError) as caught:
            self._download(_FakeGet([_FakeResponse(b"gone", status=404)]))

        self.assertIn(self.URL, str(caught.exception))
        self.assertEqual(self.sleeps, [])

    def test_an_unsupported_suffix_is_refused_before_any_request(self) -> None:
        with self.assertRaises(ValueError):
            self._download(_FakeGet([]), url="https://bucket.example/x.onnx", filename="x.onnx")


class RelineCatalogTests(_TempModelDirTestCase):
    CATALOG = b'[{"filename": "2x_a.pth", "url": "https://bucket.example/2x_a.pth"}, 7]'

    def _fetch(self, get: _FakeGet, **kwargs):
        return reline_service._fetch_catalog(get=get, sleep=self.sleeps.append, **kwargs)

    def test_a_transient_failure_is_retried_and_non_objects_are_dropped(self) -> None:
        get = _FakeGet([_FakeResponse(b"", status=503), _FakeResponse(self.CATALOG)])

        entries = self._fetch(get)

        self.assertEqual(entries, [{"filename": "2x_a.pth", "url": "https://bucket.example/2x_a.pth"}])
        self.assertEqual(len(get.headers), 2)
        self.assertEqual(get.headers[1].get("User-Agent"), reline_service.USER_AGENT)
        self.assertGreater(sum(self.sleeps), 0)

    def test_exhausted_retries_are_a_runtime_error(self) -> None:
        get = _FakeGet([_FakeResponse(b"", status=502) for _ in range(6)])

        with self.assertRaises(RuntimeError) as caught:
            self._fetch(get)
        self.assertIn(reline_service.CATALOG_URL, str(caught.exception))

    def test_a_response_that_is_not_a_json_array_is_refused(self) -> None:
        for body in (b'{"not": "a list"}', b"<html>"):
            with self.subTest(body=body):
                with self.assertRaises(RuntimeError):
                    self._fetch(_FakeGet([_FakeResponse(body)]))
        self.assertEqual(self.sleeps, [])

    def test_a_cancel_stops_the_fetch(self) -> None:
        from modules.ai_backend.engines.model_download import DownloadCancelled

        with self.assertRaises(DownloadCancelled):
            self._fetch(_FakeGet([]), should_cancel=lambda: True)


if __name__ == "__main__":
    unittest.main()
