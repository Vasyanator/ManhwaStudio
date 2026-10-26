"""
File: modules/ai_backend/inpaint/test_flux3_download.py

Purpose:
Unit tests of `flux3_download.py` over the shared `hf_gated_download`
machinery, with injected listings and a fake transport (no network): the
manifest per format (the selected DiT plus the shared AE and text encoder,
never another format's DiT, `assets/` or sample files), the format echo and
the refusal of an unknown format, the gated `no_token` short-circuit with no
network call, a whole download into the destination root with the token on
the wire, and a resume over a staged `.part`.

Notes:
The fake transports come from `_hf_download_test_fakes.py` (one implementation
of the `requests` stand-ins). Files go to temporary directories.
"""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from typing import Any

from modules.ai_backend.inpaint import flux3_download as f3
from modules.ai_backend.inpaint import hf_gated_download as hgd
from modules.ai_backend.inpaint.flux3_klein.config import WeightFormat

from modules.ai_backend.inpaint._hf_download_test_fakes import _FakeRequests, _FakeRequestsModule, _RangeRequests

#: The repository's layout (as the local `hf download` tree shows it), tiny sizes.
LISTING = {
    ".gitattributes": 100,
    "README.md": 100,
    "teaser.jpg": 100,
    "launch-gallery.jpg": 100,
    "assets/balloons.png": 100,
    "ae.safetensors": 4096,
    "config.json": 512,
    "manifest.json": 512,
    "model.safetensors": 4096,
    "text_encoder/config.json": 512,
    "text_encoder/README.md": 100,
    "text_encoder/LICENSE": 512,
    "text_encoder/model-00001-of-00002.safetensors": 4096,
    "text_encoder/model-00002-of-00002.safetensors": 4096,
    "text_encoder/model.safetensors.index.json": 512,
    "text_encoder/tokenizer.json": 512,
    "variants/fp8r/config.json": 512,
    "variants/fp8r/hf_quant_config.json": 512,
    "variants/fp8r/manifest.json": 512,
    "variants/fp8r/model.safetensors": 4096,
    "variants/nvfp4/config.json": 512,
    "variants/nvfp4/hf_quant_config.json": 512,
    "variants/nvfp4/manifest.json": 512,
    "variants/nvfp4/model.safetensors": 4096,
}

SHARED = {"ae.safetensors", "text_encoder/config.json", "text_encoder/LICENSE", "text_encoder/model-00001-of-00002.safetensors", "text_encoder/model-00002-of-00002.safetensors", "text_encoder/model.safetensors.index.json", "text_encoder/tokenizer.json"}


class _Case(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name) / f3.DIR_NAME
        self._saved = (f3.model_root, hgd.fetch_listings, hgd.probe_repo_access)
        f3.model_root = lambda: self.root  # type: ignore[assignment]
        hgd.fetch_listings = lambda repos, token: {repo: LISTING for repo in repos}  # type: ignore[assignment]

    def tearDown(self) -> None:
        f3.model_root, hgd.fetch_listings, hgd.probe_repo_access = self._saved  # type: ignore[assignment]
        self._tmp.cleanup()

    def _plan(self, weight_format: WeightFormat) -> set[str]:
        return {item.label for item in hgd.resolve_plan({f3.REPO: LISTING}, f3.manifest_entries(weight_format), self.root)}


class ManifestTests(_Case):
    def test_each_format_fetches_its_dit_and_the_shared_files_only(self) -> None:
        self.assertEqual(self._plan(WeightFormat.BF16), SHARED | {"model.safetensors", "config.json", "manifest.json"})
        for variant in (WeightFormat.FP8R, WeightFormat.NVFP4):
            with self.subTest(format=variant.value):
                folder = f"variants/{variant.value}"
                self.assertEqual(self._plan(variant), SHARED | {f"{folder}/config.json", f"{folder}/hf_quant_config.json", f"{folder}/manifest.json", f"{folder}/model.safetensors"})

    def test_the_format_and_its_refusal(self) -> None:
        self.assertIs(f3.resolve_format(None), WeightFormat.FP8R)
        self.assertIs(f3.resolve_format(""), WeightFormat.FP8R)
        self.assertIs(f3.resolve_format("nvfp4"), WeightFormat.NVFP4)
        for bad in ("fp8", "BF16", 3, True):
            with self.subTest(value=bad), self.assertRaises(ValueError):
                f3.resolve_format(bad)

    def test_the_destination_is_the_package_root(self) -> None:
        f3.model_root = self._saved[0]  # type: ignore[assignment]
        self.assertEqual(f3.model_root().name, "FLUX.3-image-klein")
        self.assertEqual(f3.model_root().parent.name, "side_models")


class CheckTests(_Case):
    def test_no_token_makes_no_network_call(self) -> None:
        def explode(*args: Any, **kwargs: Any) -> Any:
            raise AssertionError("a gated repository must not be contacted without a token")

        hgd.probe_repo_access = explode  # type: ignore[assignment]
        hgd.fetch_listings = explode  # type: ignore[assignment]
        answer = f3.check_access("  ", weight_format="nvfp4")
        self.assertEqual(answer, {"format": "nvfp4", "repos": {f3.REPO: {"state": hgd.STATE_NO_TOKEN, "message": ""}}, "plan": None, "plan_error": ""})
        with self.assertRaises(ValueError):
            f3.download("", weight_format="nvfp4")

    def test_the_answer_echoes_the_format_and_plans_it(self) -> None:
        sent: list[str] = []
        hgd.probe_repo_access = lambda repo, token: sent.append(token) or (hgd.STATE_OK, "")  # type: ignore[assignment]
        answer = f3.check_access("hf_token", weight_format="bf16")
        self.assertEqual(answer["format"], "bf16")
        self.assertEqual(answer["repos"][f3.REPO]["state"], hgd.STATE_OK)
        expected = sum(LISTING[path] for path in SHARED | {"model.safetensors", "config.json", "manifest.json"})
        self.assertEqual(answer["plan"], {"total_bytes": expected, "missing_bytes": expected, "missing_files": len(SHARED) + 3})
        self.assertEqual(sent, ["hf_token"], "the gated repository is sent the token")
        self.assertEqual(f3.check_access("hf_token")["format"], "fp8r")
        with self.assertRaises(ValueError):
            f3.check_access("hf_token", weight_format="fp16")

    def test_a_refused_repository_has_no_plan(self) -> None:
        hgd.probe_repo_access = lambda repo, token: (hgd.STATE_NOT_ACCEPTED, "403")  # type: ignore[assignment]
        answer = f3.check_access("hf_token")
        self.assertEqual(answer["repos"][f3.REPO]["state"], hgd.STATE_NOT_ACCEPTED)
        self.assertIsNone(answer["plan"])


class DownloadTests(_Case):
    def test_a_whole_download_lands_in_the_root_with_the_token(self) -> None:
        frames: list[tuple[str, int, int]] = []
        fake = _FakeRequests(lambda url: b"x" * next(size for path, size in LISTING.items() if url.endswith(path)), 1024)
        with _FakeRequestsModule(fake):
            result = f3.download("hf_token", weight_format="fp8r", progress_callback=lambda phase, step, total, label, **extra: frames.append((phase, step, total)))
        self.assertEqual(result["root"], str(self.root))
        self.assertEqual(result["skipped_files"], 0)
        self.assertEqual({headers.get("Authorization") for _url, headers in fake.calls}, {"Bearer hf_token"})
        for label in self._plan(WeightFormat.FP8R):
            self.assertEqual((self.root / label).stat().st_size, LISTING[label], label)
        self.assertFalse((self.root / "variants" / "nvfp4").exists())
        self.assertFalse((self.root / "model.safetensors").exists())
        self.assertEqual(frames[-1][0], "download")
        self.assertEqual(frames[-1][1], frames[-1][2])

    def test_a_staged_part_is_resumed(self) -> None:
        plan = hgd.resolve_plan({f3.REPO: LISTING}, f3.manifest_entries(WeightFormat.NVFP4), self.root)
        target = next(item for item in plan if item.label == "variants/nvfp4/model.safetensors")
        for item in plan:
            if item is not target:
                Path(item.dest).parent.mkdir(parents=True, exist_ok=True)
                Path(item.dest).write_bytes(b"d" * item.size)
        full = bytes(range(256)) * 16
        parked = Path(target.dest).with_name(Path(target.dest).name + ".part")
        parked.parent.mkdir(parents=True, exist_ok=True)
        parked.write_bytes(full[:1000])
        transport = _RangeRequests(full)
        with _FakeRequestsModule(transport):
            result = f3.download("hf_token", weight_format="nvfp4")
        self.assertEqual(transport.ranged, ["bytes=1000-"])
        self.assertEqual(Path(target.dest).read_bytes(), full)
        self.assertEqual(result["skipped_files"], len(plan) - 1)

    def test_a_cancel_stops_the_transfer(self) -> None:
        with _FakeRequestsModule(_FakeRequests(lambda url: b"x" * 4096, 512)), self.assertRaises(f3.DownloadCanceled):
            f3.download("hf_token", should_cancel=lambda: True)


if __name__ == "__main__":
    unittest.main()
