"""
File: modules/ai_backend/watermark/test_service.py

Purpose:
Unit tests for `WatermarkRemovalService`: parameter contracts, model residency,
the checkpoint integrity gate, the tiling geometry and the detect-pass mask
round trip.

Main responsibilities:
- verify detect/remove parameter normalization, clamping and the refusal to
  silently substitute an unknown model;
- verify the model-key swap reports `mark_unloaded` for the previous key, that
  `unload()` reports true/false correctly and that `_unload_key` refuses a
  foreign key;
- verify the checkpoint magic-byte gate rejects a Google Drive HTML interstitial
  and accepts both legacy-pickle and zip checkpoints;
- verify `_load_state_dict` requests `weights_only=True` explicitly, unwraps
  `state_dict`, strips a `module.` prefix and turns torch's restricted-unpickler
  refusal into an explicit message rather than a blanket `weights_only=False`;
- verify two concurrent first-use requests download the checkpoint exactly once,
  into a process-private staging file, and leave nothing behind on failure;
- verify a failed forward is reported as a failed INFERENCE, not a failed load:
  the network stays resident, counted and evictable;
- verify the tiling geometry (tile count, full coverage) and that the
  cosine feather is a partition of unity across an overlap band;
- verify the downscale -> pad -> crop -> upscale mask round trip preserves the
  source resolution and the location of the marked region.

Notes:
- A fake `torch` module is injected into `sys.modules` with `patch.dict` +
  `addCleanup` where torch is needed at all, so the tests need neither torch,
  nor weights, nor a GPU, nor the network. `numpy` and `Pillow` are real (they
  are hard dependencies of the backend and are cheap).
- The model root is redirected into a temporary directory, so no test writes
  into the user's model tree.
"""

from __future__ import annotations

import os
import sys
import tempfile
import threading
import time
import types
import unittest
from pathlib import Path
from unittest.mock import patch

import numpy as np

from modules.ai_backend.runtime.model_manager import LoadedModelManager
from modules.ai_backend.watermark import code_fetch as cf
from modules.ai_backend.watermark import service as svc


# =====================================================================
#  Parameters
# =====================================================================
class NormalizeDetectParamsTests(unittest.TestCase):
    def test_defaults(self) -> None:
        out = svc.normalize_detect_params(None)
        self.assertEqual(
            out,
            {
                "model": svc.DEFAULT_MODEL,
                "downscale_to": svc.DEFAULT_DOWNSCALE,
                "threshold": 0.5,
                "dilate_px": 4,
            },
        )

    def test_downscale_snaps_to_an_allowed_option(self) -> None:
        self.assertEqual(svc.normalize_detect_params({"downscale_to": 768})["downscale_to"], 768)
        self.assertEqual(svc.normalize_detect_params({"downscale_to": 999})["downscale_to"], 512)
        self.assertEqual(svc.normalize_detect_params({"downscale_to": "256"})["downscale_to"], 256)

    def test_numeric_clamping(self) -> None:
        out = svc.normalize_detect_params({"threshold": 9.0, "dilate_px": 5000})
        self.assertAlmostEqual(out["threshold"], 1.0)
        self.assertEqual(out["dilate_px"], 64)

        out = svc.normalize_detect_params({"threshold": -3.0, "dilate_px": -7})
        self.assertAlmostEqual(out["threshold"], 0.0)
        self.assertEqual(out["dilate_px"], 0)

    def test_garbage_falls_back_to_the_documented_defaults(self) -> None:
        out = svc.normalize_detect_params({"threshold": "abc", "dilate_px": None})
        self.assertAlmostEqual(out["threshold"], 0.5)
        self.assertEqual(out["dilate_px"], 4)

    def test_nan_threshold_falls_back_instead_of_clamping(self) -> None:
        out = svc.normalize_detect_params({"threshold": float("nan")})
        self.assertAlmostEqual(out["threshold"], 0.5)

    def test_unknown_model_raises_instead_of_falling_back(self) -> None:
        with self.assertRaises(ValueError):
            svc.normalize_detect_params({"model": "sd15"})

    def test_empty_model_means_the_default(self) -> None:
        self.assertEqual(svc.normalize_detect_params({"model": "  "})["model"], svc.DEFAULT_MODEL)
        self.assertEqual(svc.normalize_detect_params({"model": "SplitNet"})["model"], "splitnet")


class NormalizeRemoveParamsTests(unittest.TestCase):
    def test_defaults(self) -> None:
        out = svc.normalize_remove_params(None)
        self.assertEqual(out["model"], svc.DEFAULT_MODEL)
        self.assertEqual(out["tile"], svc.DEFAULT_TILE)
        self.assertEqual(out["overlap"], svc.DEFAULT_OVERLAP)

    def test_tile_is_snapped_down_onto_the_16_grid(self) -> None:
        # SLBR and SplitNet reject any spatial size that is not a multiple of 16.
        for requested in (129, 200, 511, 1000):
            with self.subTest(tile=requested):
                tile = svc.normalize_remove_params({"tile": requested})["tile"]
                self.assertEqual(tile % 16, 0)
                self.assertLessEqual(tile, requested)
                self.assertGreaterEqual(tile, svc.MIN_TILE)

    def test_tile_is_clamped_to_the_supported_range(self) -> None:
        self.assertEqual(svc.normalize_remove_params({"tile": 1})["tile"], svc.MIN_TILE)
        self.assertEqual(svc.normalize_remove_params({"tile": 99999})["tile"], svc.MAX_TILE)

    def test_overlap_can_never_consume_a_whole_tile(self) -> None:
        out = svc.normalize_remove_params({"tile": 256, "overlap": 9999})
        self.assertEqual(out["overlap"], 128)
        self.assertEqual(svc.normalize_remove_params({"overlap": -5})["overlap"], 0)

    def test_unknown_model_raises(self) -> None:
        with self.assertRaises(ValueError):
            svc.normalize_remove_params({"model": "lama"})


# =====================================================================
#  Checkpoint integrity
# =====================================================================
class CheckpointMagicTests(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)

    def _write(self, payload: bytes) -> Path:
        path = self.dir / "model_best.pth.tar"
        path.write_bytes(payload)
        return path

    def test_html_interstitial_is_rejected_with_a_pointed_message(self) -> None:
        path = self._write(
            b"<!DOCTYPE html><html><head><title>Google Drive - Virus scan warning</title>"
        )
        with self.assertRaises(RuntimeError) as caught:
            svc._validate_checkpoint_magic(path, "slbr")
        self.assertIn("Google Drive", str(caught.exception))

    def test_bare_html_tag_is_rejected(self) -> None:
        path = self._write(b"<html><body>quota exceeded</body></html>")
        with self.assertRaises(RuntimeError):
            svc._validate_checkpoint_magic(path, "wdnet")

    def test_arbitrary_garbage_is_rejected(self) -> None:
        path = self._write(b"not a checkpoint at all")
        with self.assertRaises(RuntimeError) as caught:
            svc._validate_checkpoint_magic(path, "splitnet")
        self.assertIn("повреждён", str(caught.exception))

    def test_legacy_pickle_checkpoint_is_accepted(self) -> None:
        # torch's legacy (pre-1.6) save is a protocol-2 pickle stream.
        svc._validate_checkpoint_magic(self._write(b"\x80\x02}q\x00."), "slbr")

    def test_zip_checkpoint_is_accepted(self) -> None:
        svc._validate_checkpoint_magic(self._write(b"PK\x03\x04\x00\x00\x00\x00"), "slbr")

    def test_missing_file_is_an_explicit_error(self) -> None:
        with self.assertRaises(RuntimeError):
            svc._validate_checkpoint_magic(self.dir / "absent.pth.tar", "slbr")

    def test_no_digest_is_pinned_yet_and_none_is_fabricated(self) -> None:
        # The digests could not be captured (Drive was unreachable); `None` means
        # "unverified" and must never be replaced by an invented value.
        self.assertEqual(set(svc._WEIGHT_SHA256), set(svc.MODEL_IDS))
        for model_id, digest in svc._WEIGHT_SHA256.items():
            with self.subTest(model=model_id):
                self.assertTrue(digest is None or len(digest) == 64)

    def test_unverified_digest_skips_the_comparison(self) -> None:
        path = self._write(b"\x80\x02}q\x00.")
        with self.assertLogs(svc.log, level="INFO"):
            svc._verify_optional_sha256(path, "slbr")

    def test_known_digest_mismatch_is_fatal(self) -> None:
        path = self._write(b"\x80\x02}q\x00.")
        with patch.dict(svc._WEIGHT_SHA256, {"slbr": "0" * 64}):
            with self.assertRaises(RuntimeError):
                svc._verify_optional_sha256(path, "slbr")


class ConfirmUrlTests(unittest.TestCase):
    """Drive's >100 MB interstitial carries the token and a per-session uuid."""

    HTML = (
        '<form action="https://drive.usercontent.google.com/download">'
        '<input type="hidden" name="id" value="X">'
        '<input type="hidden" name="confirm" value="t">'
        '<input type="hidden" name="uuid" value="abc-123">'
        "</form>"
    )

    def test_confirm_and_uuid_are_appended(self) -> None:
        url = svc._build_confirm_url("https://example/download?id=X", self.HTML)
        self.assertEqual(url, "https://example/download?id=X&confirm=t&uuid=abc-123")

    def test_unparsable_form_still_sends_confirm_t(self) -> None:
        url = svc._build_confirm_url("https://example/download?id=X", "<html>nope</html>")
        self.assertEqual(url, "https://example/download?id=X&confirm=t")


class _FakeDriveResponse:
    """Stand-in for `requests.Response` from Google Drive."""

    def __init__(self, payload: bytes, content_type: str) -> None:
        self._payload = payload
        self.headers = {"Content-Type": content_type, "Content-Length": str(len(payload))}

    def __enter__(self) -> "_FakeDriveResponse":
        return self

    def __exit__(self, *_exc_info: object) -> bool:
        return False

    def raise_for_status(self) -> None:
        return None

    @property
    def text(self) -> str:
        return self._payload.decode("utf-8", "replace")

    def iter_content(self, chunk_size: int = 1 << 20):
        for start in range(0, len(self._payload), chunk_size):
            yield self._payload[start : start + chunk_size]


def _fake_drive_requests(responses: dict[str, _FakeDriveResponse], seen: list[str]):
    """A `requests` module whose `Session.get` serves `responses[url]`."""

    class Session:
        def __enter__(self) -> "Session":
            return self

        def __exit__(self, *_exc_info: object) -> bool:
            return False

        def get(self, url: str, **_kwargs: object) -> _FakeDriveResponse:
            seen.append(url)
            if url not in responses:
                raise AssertionError(f"unexpected request: {url}")
            return responses[url]

    module = types.ModuleType("requests")
    module.Session = Session
    return module


class DriveDownloadTests(unittest.TestCase):
    """The confirm flow and the HTML-interstitial backstop, without the network."""

    FILE_ID = "TESTID"
    PAYLOAD = b"\x80\x02" + b"weights" * 100

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dest = Path(tmp.name) / "model_best.pth.tar.part"
        self.base = svc._DRIVE_URL.format(file_id=self.FILE_ID)
        self.seen: list[str] = []
        self.progress: list[tuple[int, int]] = []

    def _download(self, responses: dict[str, _FakeDriveResponse]) -> None:
        module = _fake_drive_requests(responses, self.seen)
        with patch.dict(sys.modules, {"requests": module}):
            svc._download_from_google_drive(
                self.FILE_ID, self.dest, lambda done, expected: self.progress.append((done, expected))
            )

    def test_plain_get_is_used_when_drive_serves_the_payload(self) -> None:
        self._download({self.base: _FakeDriveResponse(self.PAYLOAD, "application/octet-stream")})

        self.assertEqual(self.seen, [self.base])
        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)
        self.assertEqual(self.progress[-1], (len(self.PAYLOAD), len(self.PAYLOAD)))

    def test_confirm_flow_is_followed_for_the_large_file(self) -> None:
        html = (
            b'<form><input name="confirm" value="t">'
            b'<input name="uuid" value="u-1"></form>'
        )
        confirm_url = f"{self.base}&confirm=t&uuid=u-1"
        self._download(
            {
                self.base: _FakeDriveResponse(html, "text/html; charset=utf-8"),
                confirm_url: _FakeDriveResponse(self.PAYLOAD, "application/octet-stream"),
            }
        )

        self.assertEqual(self.seen, [self.base, confirm_url])
        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)

    def test_a_second_html_answer_is_a_loud_failure(self) -> None:
        html = b"<html>Quota exceeded</html>"
        confirm_url = f"{self.base}&confirm=t"
        with self.assertRaises(RuntimeError) as caught:
            self._download(
                {
                    self.base: _FakeDriveResponse(html, "text/html"),
                    confirm_url: _FakeDriveResponse(html, "text/html"),
                }
            )

        self.assertIn("HTML", str(caught.exception))
        # Nothing was written: an interstitial must never reach the disk as a
        # checkpoint.
        self.assertFalse(self.dest.exists())

    def test_transport_failure_is_wrapped_with_context(self) -> None:
        module = types.ModuleType("requests")

        class Session:
            def __enter__(self) -> "Session":
                return self

            def __exit__(self, *_exc_info: object) -> bool:
                return False

            def get(self, *_args: object, **_kwargs: object) -> None:
                raise ConnectionError("name resolution failed")

        module.Session = Session
        with patch.dict(sys.modules, {"requests": module}):
            with self.assertRaises(RuntimeError) as caught:
                svc._download_from_google_drive(self.FILE_ID, self.dest, lambda *_: None)

        self.assertIn(self.FILE_ID, str(caught.exception))


class WeightDownloadConcurrencyTests(unittest.TestCase):
    """The IPC layer dispatches onto a thread pool, so two first uses can race."""

    MODEL = "slbr"
    PAYLOAD = b"\x80\x02" + b"weights" * 4096

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        root_patch = patch.object(cf, "_watermark_dir", lambda: self.root)
        root_patch.start()
        self.addCleanup(root_patch.stop)

        self.staging: list[Path] = []
        self.service = svc.WatermarkRemovalService(LoadedModelManager(max_loaded_models=1))

    def _fake_download(self, _file_id: str, dest: Path, on_chunk) -> None:
        """Write the payload slowly, so an unserialized second writer interleaves."""
        self.staging.append(dest)
        with dest.open("wb") as handle:
            for start in range(0, len(self.PAYLOAD), 4096):
                handle.write(self.PAYLOAD[start : start + 4096])
                time.sleep(0.01)
        on_chunk(len(self.PAYLOAD), len(self.PAYLOAD))

    def test_two_concurrent_first_uses_download_once_and_do_not_corrupt(self) -> None:
        errors: list[BaseException] = []

        def worker() -> None:
            try:
                self.service._ensure_weights(self.MODEL, None)
            except BaseException as exc:  # noqa: BLE001 - re-raised by the assertion below
                errors.append(exc)

        with patch.object(svc, "_download_from_google_drive", self._fake_download):
            threads = [threading.Thread(target=worker) for _ in range(2)]
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join(timeout=30)

        self.assertEqual(errors, [])
        # The loser of the lock re-checks and skips the 80-130 MiB refetch.
        self.assertEqual(len(self.staging), 1)
        self.assertEqual(svc.weights_path(self.MODEL).read_bytes(), self.PAYLOAD)
        self.assertEqual(list((self.root / self.MODEL).glob("*.part")), [])

    def test_the_staging_file_is_process_private(self) -> None:
        # Threads are serialized by the lock; a second PROCESS is not, so the
        # staging name must not be shared either.
        with patch.object(svc, "_download_from_google_drive", self._fake_download):
            self.service._ensure_weights(self.MODEL, None)

        self.assertEqual(len(self.staging), 1)
        self.assertIn(str(os.getpid()), self.staging[0].name)
        self.assertTrue(self.staging[0].name.endswith(".part"))

    def test_a_failed_download_leaves_no_staging_file_behind(self) -> None:
        def boom(_file_id: str, dest: Path, _on_chunk) -> None:
            self.staging.append(dest)
            dest.write_bytes(b"<!DOCTYPE html>partial")
            raise RuntimeError("transport died")

        with patch.object(svc, "_download_from_google_drive", boom):
            with self.assertRaises(RuntimeError):
                self.service._ensure_weights(self.MODEL, None)

        self.assertFalse(self.staging[0].exists())
        self.assertFalse(svc.are_weights_ready(self.MODEL))

    def test_an_html_interstitial_never_becomes_the_checkpoint(self) -> None:
        def html(_file_id: str, dest: Path, _on_chunk) -> None:
            self.staging.append(dest)
            dest.write_bytes(b"<!DOCTYPE html><html>Google Drive</html>")

        with patch.object(svc, "_download_from_google_drive", html):
            with self.assertRaises(RuntimeError):
                self.service._ensure_weights(self.MODEL, None)

        self.assertFalse(svc.weights_path(self.MODEL).exists())
        self.assertFalse(self.staging[0].exists())

    def test_a_present_checkpoint_is_not_re_downloaded(self) -> None:
        path = svc.weights_path(self.MODEL)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(self.PAYLOAD)

        def unexpected(*_args: object, **_kwargs: object) -> None:
            raise AssertionError("the checkpoint was already on disk")

        with patch.object(svc, "_download_from_google_drive", unexpected):
            self.service._ensure_weights(self.MODEL, None)


# =====================================================================
#  Checkpoint reading (fake torch)
# =====================================================================
def _install_fake_torch(
    load_result: object, calls: list[dict[str, object]] | None = None
) -> types.ModuleType:
    """Inject a `torch` whose `load()` returns (or raises) `load_result`.

    `calls`, when given, records the keyword arguments of every `load()` call.
    """

    def load(_path: object, **kwargs: object) -> object:
        if calls is not None:
            calls.append(dict(kwargs))
        if isinstance(load_result, Exception):
            raise load_result
        return load_result

    module = types.ModuleType("torch")
    module.load = load
    return module


class LoadStateDictTests(unittest.TestCase):
    def _load(
        self,
        payload: object,
        model_id: str = "slbr",
        calls: list[dict[str, object]] | None = None,
    ) -> dict[str, object]:
        fake = _install_fake_torch(payload, calls)
        with patch.dict(sys.modules, {"torch": fake}):
            return svc._load_state_dict(Path("/nowhere/model_best.pth.tar"), model_id)

    def test_weights_only_is_requested_explicitly(self) -> None:
        # Not inherited from the installed torch's default: `weights_only=True`
        # only became the default in torch 2.6, and these checkpoints come from
        # third-party Google Drive links.
        calls: list[dict[str, object]] = []
        self._load({"state_dict": {"w": 1}}, calls=calls)
        self.assertEqual(len(calls), 1)
        self.assertIs(calls[0].get("weights_only"), True)
        self.assertEqual(calls[0].get("map_location"), "cpu")

    def test_state_dict_entry_is_unwrapped(self) -> None:
        out = self._load({"epoch": 40, "state_dict": {"encoder.weight": 1}, "best_acc": 0.9})
        self.assertEqual(out, {"encoder.weight": 1})

    def test_bare_state_dict_is_accepted(self) -> None:
        out = self._load({"inc.weight": 2}, "wdnet")
        self.assertEqual(out, {"inc.weight": 2})

    def test_module_prefix_is_stripped_defensively(self) -> None:
        out = self._load({"state_dict": {"module.encoder.weight": 3, "decoder.bias": 4}})
        self.assertEqual(out, {"encoder.weight": 3, "decoder.bias": 4})

    def test_restricted_unpickler_error_is_explained_not_bypassed(self) -> None:
        error = RuntimeError(
            "Weights only load failed. Re-running `torch.load` with `weights_only=False`"
        )
        with self.assertRaises(RuntimeError) as caught:
            self._load(error)
        message = str(caught.exception)
        self.assertIn("weights_only=True", message)
        self.assertIn("небезопасном режиме", message)

    def test_non_mapping_payload_is_rejected(self) -> None:
        with self.assertRaises(RuntimeError):
            self._load([1, 2, 3])

    def test_non_mapping_state_dict_field_is_rejected(self) -> None:
        with self.assertRaises(RuntimeError):
            self._load({"state_dict": "oops"})


# =====================================================================
#  Residency
# =====================================================================
class _FakeNet:
    """Stand-in for a constructed `nn.Module`: records `eval()` and the device."""

    def __init__(self, model_id: str) -> None:
        self.model_id = model_id
        self.eval_calls = 0
        self.moved_to: list[object] = []
        self.loaded: dict[str, object] | None = None

    def eval(self) -> "_FakeNet":
        self.eval_calls += 1
        return self

    def load_state_dict(self, state_dict: dict[str, object]) -> None:
        self.loaded = state_dict


class ResidencyTests(unittest.TestCase):
    """Cover the lease protocol and the single-resident-model discipline."""

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)

        for model_id in svc.MODEL_IDS:
            path = self.root / model_id / svc.WEIGHTS[model_id].file_name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"\x80\x02}q\x00.")

        self.built: list[_FakeNet] = []
        self.moves: list[tuple[object, object]] = []

        def build_network(model_id: str) -> _FakeNet:
            net = _FakeNet(model_id)
            self.built.append(net)
            return net

        def move_module_to(module: object, device: object) -> object:
            self.moves.append((module, device))
            return module

        for target, name, replacement in (
            (cf, "_watermark_dir", lambda: self.root),
            (cf, "build_network", build_network),
            (svc, "move_module_to", move_module_to),
            (svc, "_load_state_dict", lambda _path, _model: {"w": 1}),
            (svc, "_clear_torch_cache", lambda: None),
            (svc, "_resolve_selected_backend_device", lambda _fallback: "cpu"),
        ):
            attr_patch = patch.object(target, name, replacement)
            attr_patch.start()
            self.addCleanup(attr_patch.stop)

        self.service = svc.WatermarkRemovalService(LoadedModelManager(max_loaded_models=4))

    def test_first_use_loads_evaluates_and_stages_the_weights(self) -> None:
        result = self.service._lease_and_run("slbr", lambda net: net.model_id)

        self.assertEqual(result, "slbr")
        self.assertEqual(len(self.built), 1)
        self.assertEqual(self.built[0].eval_calls, 1)
        self.assertEqual(self.built[0].loaded, {"w": 1})
        self.assertEqual(self.moves, [(self.built[0], "cpu")])
        self.assertEqual(self.service._active_key, "watermark:slbr:cpu")

    def test_repeated_use_reuses_the_resident_network(self) -> None:
        self.service._lease_and_run("slbr", lambda net: net)
        self.service._lease_and_run("slbr", lambda net: net)
        self.assertEqual(len(self.built), 1)

    def test_model_swap_reports_the_previous_key_as_unloaded(self) -> None:
        self.service._lease_and_run("slbr", lambda net: net)

        with patch.object(self.service._model_manager, "mark_unloaded") as unloaded:
            self.service._lease_and_run("wdnet", lambda net: net)

        unloaded.assert_called_once_with("watermark:slbr:cpu")
        self.assertEqual(self.service._active_key, "watermark:wdnet:cpu")
        self.assertEqual(len(self.built), 2)

    def test_health_reflects_the_resident_model(self) -> None:
        self.assertFalse(self.service.health()["ready"])
        self.service._lease_and_run("splitnet", lambda net: net)

        health = self.service.health()
        self.assertTrue(health["ready"])
        self.assertEqual(health["model"], "splitnet")
        self.assertEqual(health["device"], "cpu")
        self.assertEqual(health["active_key"], "watermark:splitnet:cpu")
        self.assertIsNone(health["last_error"])

    def test_failure_records_last_error_and_leaves_no_lease_behind(self) -> None:
        def boom(_net: object) -> None:
            raise RuntimeError("forward exploded")

        with self.assertRaises(RuntimeError):
            with self.assertLogs(svc.log, level="ERROR"):
                self.service._lease_and_run("slbr", boom)

        self.assertEqual(self.service.health()["last_error"], "forward exploded")
        # `release()` ran in the `finally` branch, so nothing stays leased.
        self.assertEqual(self.service._model_manager.health()["active_model_count"], 0)

    def test_a_failed_forward_leaves_the_network_resident(self) -> None:
        """A failed INFERENCE is not a failed LOAD.

        `mark_load_failed()` reaches `abort_load`, which clears `resident` and
        drops the entry's unload callback — while `_net` still holds the network
        and its weights still occupy the device. The manager would then
        under-count residency and never be able to evict it again.
        """

        def boom(_net: object) -> None:
            raise RuntimeError("forward exploded")

        with self.assertRaises(RuntimeError):
            with self.assertLogs(svc.log, level="ERROR"):
                self.service._lease_and_run("slbr", boom)

        self.assertIsNotNone(self.service._net)
        health = self.service._model_manager.health()
        self.assertEqual(health["resident_model_count"], 1)
        self.assertEqual(health["active_model_count"], 0)
        self.assertEqual(health["loading_model_count"], 0)

    def test_a_network_left_by_a_failed_forward_is_still_evictable(self) -> None:
        service = svc.WatermarkRemovalService(LoadedModelManager(max_loaded_models=1))

        def boom(_net: object) -> None:
            raise RuntimeError("forward exploded")

        with self.assertRaises(RuntimeError):
            with self.assertLogs(svc.log, level="ERROR"):
                service._lease_and_run("slbr", boom)

        # Another domain now needs the single resident slot, so the entry must
        # still carry this service's unload callback.
        lease = service._model_manager.begin_model_use("other_domain:model")
        self.addCleanup(lease.release)

        self.assertTrue(lease.needs_load)
        self.assertIsNone(service._net, "the eviction callback should have dropped the network")

    def test_unload_drops_the_network_and_reports_it(self) -> None:
        self.service._lease_and_run("slbr", lambda net: net)

        with patch.object(self.service._model_manager, "mark_unloaded") as unloaded:
            self.assertTrue(self.service.unload())

        unloaded.assert_called_once_with("watermark:slbr:cpu")
        self.assertIsNone(self.service._net)
        self.assertIsNone(self.service._active_key)

    def test_unload_without_a_network_is_a_noop(self) -> None:
        self.assertFalse(self.service.unload())

    def test_unload_key_refuses_a_foreign_key(self) -> None:
        self.service._lease_and_run("slbr", lambda net: net)

        self.assertFalse(self.service._unload_key("watermark:wdnet:cpu"))
        self.assertIsNotNone(self.service._net)
        self.assertTrue(self.service._unload_key("watermark:slbr:cpu"))
        self.assertIsNone(self.service._net)

    def test_missing_checkpoint_is_an_explicit_error(self) -> None:
        (self.root / "slbr" / svc.WEIGHTS["slbr"].file_name).unlink()

        with self.assertRaises(FileNotFoundError):
            with self.assertLogs(svc.log, level="ERROR"):
                self.service._lease_and_run("slbr", lambda net: net)

    def test_html_interstitial_on_disk_blocks_the_load(self) -> None:
        (self.root / "slbr" / svc.WEIGHTS["slbr"].file_name).write_bytes(b"<!DOCTYPE html>")

        with self.assertRaises(RuntimeError):
            with self.assertLogs(svc.log, level="ERROR"):
                self.service._lease_and_run("slbr", lambda net: net)

    def test_status_lists_the_catalog_and_what_is_on_disk(self) -> None:
        status = self.service.status()
        self.assertEqual([m["id"] for m in status["models"]], list(svc.MODEL_IDS))
        self.assertEqual(status["default_model"], svc.DEFAULT_MODEL)
        self.assertEqual(sorted(status["downloaded_models"]), sorted(svc.MODEL_IDS))
        # The network code has not been fetched into the temp root.
        self.assertEqual(status["code_ready_models"], [])


# =====================================================================
#  Geometry
# =====================================================================
class TilingGeometryTests(unittest.TestCase):
    def test_single_tile_when_the_area_fits(self) -> None:
        self.assertEqual(svc._plan_tiles(512, 512, 512, 64), [(0, 0)])
        self.assertEqual(svc._axis_offsets(300, 512, 64), [0])

    def test_offsets_are_strided_and_flushed_against_the_far_edge(self) -> None:
        offsets = svc._axis_offsets(1000, 512, 64)
        self.assertEqual(offsets[0], 0)
        self.assertEqual(offsets[-1], 1000 - 512)
        self.assertEqual(offsets, sorted(set(offsets)))

    def test_tile_count_is_the_product_of_the_axis_counts(self) -> None:
        rows = svc._axis_offsets(1300, 512, 64)
        cols = svc._axis_offsets(900, 512, 64)
        self.assertEqual(len(svc._plan_tiles(1300, 900, 512, 64)), len(rows) * len(cols))

    def test_every_pixel_is_covered_by_at_least_one_tile(self) -> None:
        height, width, tile, overlap = 1200, 700, 256, 64
        covered = np.zeros((height, width), dtype=bool)
        for top, left in svc._plan_tiles(height, width, tile, overlap):
            covered[top : top + tile, left : left + tile] = True
        self.assertTrue(covered.all())

    def test_zero_overlap_gives_a_plain_grid(self) -> None:
        self.assertEqual(svc._axis_offsets(1024, 256, 0), [0, 256, 512, 768])


class FeatherWindowTests(unittest.TestCase):
    def test_window_is_flat_outside_the_ramps(self) -> None:
        window = svc._feather_window(256, 64)
        self.assertEqual(window.shape, (256,))
        np.testing.assert_allclose(window[64:192], 1.0, atol=1e-6)

    def test_zero_overlap_is_a_box_window(self) -> None:
        np.testing.assert_allclose(svc._feather_window(128, 0), 1.0)

    def test_adjacent_windows_sum_to_one_across_the_overlap(self) -> None:
        tile, overlap = 256, 64
        window = svc._feather_window(tile, overlap)
        # Two tiles spaced `tile - overlap` apart: the left tile's tail ramp and
        # the right tile's head ramp must form a partition of unity.
        left_tail = window[tile - overlap :]
        right_head = window[:overlap]
        np.testing.assert_allclose(left_tail + right_head, 1.0, atol=1e-6)

    def test_accumulated_weight_is_uniform_in_the_interior(self) -> None:
        tile, overlap, extent = 256, 64, 1024
        window = svc._feather_window(tile, overlap)
        accumulated = np.zeros(extent, dtype=np.float32)
        for offset in svc._axis_offsets(extent, tile, overlap):
            accumulated[offset : offset + tile] += window
        interior = accumulated[tile:-tile]
        np.testing.assert_allclose(interior, 1.0, atol=1e-5)
        # Borders are below 1 by construction, which is exactly why the tiled
        # pass divides by the accumulated weight instead of trusting the window.
        self.assertLess(float(accumulated[0]), 1.0)


class PaddingTests(unittest.TestCase):
    def test_pad_square_multiple_produces_a_square_multiple_of_16(self) -> None:
        image = np.zeros((300, 173, 3), dtype=np.uint8)
        padded, original = svc._pad_square_multiple(image, 16)
        self.assertEqual(original, (300, 173))
        self.assertEqual(padded.shape[0], padded.shape[1])
        self.assertEqual(padded.shape[0] % 16, 0)
        self.assertGreaterEqual(padded.shape[0], 300)

    def test_padding_keeps_the_original_pixels_in_place(self) -> None:
        image = np.arange(4 * 6 * 3, dtype=np.uint8).reshape(4, 6, 3)
        padded, _ = svc._pad_square_multiple(image, 16)
        np.testing.assert_array_equal(padded[:4, :6], image)

    def test_multiple_of_one_still_pads_to_a_square(self) -> None:
        padded, _ = svc._pad_square_multiple(np.zeros((7, 3, 3), dtype=np.uint8), 1)
        self.assertEqual(padded.shape[:2], (7, 7))

    def test_a_target_smaller_than_the_input_is_not_a_crop(self) -> None:
        image = np.zeros((40, 50, 3), dtype=np.uint8)
        padded, original = svc._pad_reflect(image, 10, 10)
        self.assertEqual(padded.shape[:2], (40, 50))
        self.assertEqual(original, (40, 50))


class MaskRoundTripTests(unittest.TestCase):
    """The detect pass returns a mask at the SOURCE resolution."""

    def test_downscale_pad_crop_upscale_restores_the_source_size(self) -> None:
        source_h, source_w = 900, 400
        image = np.zeros((source_h, source_w, 3), dtype=np.uint8)

        work = svc._downscale_long_side(image, 512)
        self.assertEqual(max(work.shape[:2]), 512)
        padded, (crop_h, crop_w) = svc._pad_square_multiple(work, 16)
        self.assertEqual(padded.shape[0], padded.shape[1])

        # A synthetic prediction at the padded resolution, marking a band.
        prediction = np.zeros(padded.shape[:2], dtype=np.float32)
        prediction[: crop_h // 2, :crop_w] = 1.0

        cropped = prediction[:crop_h, :crop_w]
        upscaled = svc._resize_mask_float(cropped, source_w, source_h)
        self.assertEqual(upscaled.shape, (source_h, source_w))

        binary = svc._binarize_and_dilate(upscaled, 0.5, 0)
        self.assertEqual(binary.shape, (source_h, source_w))
        self.assertEqual(binary.dtype, np.uint8)
        # The marked half stays the marked half after the round trip.
        self.assertTrue(binary[: source_h // 2 - 8].all())
        self.assertFalse(binary[source_h // 2 + 8 :].any())

    def test_small_images_are_not_upscaled(self) -> None:
        image = np.zeros((64, 32, 3), dtype=np.uint8)
        self.assertEqual(svc._downscale_long_side(image, 512).shape, image.shape)

    def test_resize_is_a_noop_at_the_target_size(self) -> None:
        mask = np.linspace(0.0, 1.0, 16, dtype=np.float32).reshape(4, 4)
        np.testing.assert_allclose(svc._resize_mask_float(mask, 4, 4), mask)

    def test_threshold_and_dilation_produce_l8_values(self) -> None:
        mask = np.zeros((32, 32), dtype=np.float32)
        mask[16, 16] = 1.0

        tight = svc._binarize_and_dilate(mask, 0.5, 0)
        self.assertEqual(int(tight.sum()) // 255, 1)

        grown = svc._binarize_and_dilate(mask, 0.5, 3)
        self.assertGreater(int(np.count_nonzero(grown)), 1)
        self.assertEqual(set(np.unique(grown).tolist()) - {0, 255}, set())

    def test_threshold_is_inclusive_at_its_own_value(self) -> None:
        mask = np.full((4, 4), 0.5, dtype=np.float32)
        self.assertTrue((svc._binarize_and_dilate(mask, 0.5, 0) == 255).all())
        self.assertTrue((svc._binarize_and_dilate(mask, 0.75, 0) == 0).all())


class OutputSelectionTests(unittest.TestCase):
    """Each model returns a differently shaped tuple; the adapter pins them."""

    def test_slbr_takes_the_refined_image_and_the_primary_mask(self) -> None:
        outputs = (["refined", "coarse"], ["mask", "aux"], ["watermark"])
        self.assertEqual(svc._select_outputs("slbr", outputs), ("refined", "mask"))

    def test_splitnet_mask_is_a_bare_tensor(self) -> None:
        outputs = (["refined", "coarse"], "mask", "watermark")
        self.assertEqual(svc._select_outputs("splitnet", outputs), ("refined", "mask"))

    def test_wdnet_returns_five_values(self) -> None:
        outputs = ("image", "mask", "alpha", "watermark", "intermediate")
        self.assertEqual(svc._select_outputs("wdnet", outputs), ("image", "mask"))

    def test_unexpected_shape_is_an_explicit_error(self) -> None:
        with self.assertRaises(RuntimeError):
            svc._select_outputs("slbr", ("only-one",))

    def test_unknown_model_is_rejected(self) -> None:
        with self.assertRaises(ValueError):
            svc._select_outputs("bogus", ("a", "b"))


class ProgressEmitTests(unittest.TestCase):
    def test_a_raising_callback_never_aborts_the_job(self) -> None:
        def explode(*_args: object) -> None:
            raise RuntimeError("sink is gone")

        svc._emit(explode, "download", 1, 2, "label")

    def test_no_callback_is_a_noop(self) -> None:
        svc._emit(None, "generate", 1, 1, "label")

    def test_total_is_never_zero(self) -> None:
        frames: list[tuple[str, int, int, str]] = []
        svc._emit(lambda *frame: frames.append(frame), "generate", 0, 0, "label")
        self.assertEqual(frames, [("generate", 0, 1, "label")])


if __name__ == "__main__":
    unittest.main()
