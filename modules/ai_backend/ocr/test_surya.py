"""
File: modules/ai_backend/ocr/test_surya.py

Purpose:
Unit tests for the Surya OCR predictor loading contract: each predictor loads
from the local checkpoint directory of the request (passed as `checkpoint=`
and assigned to Surya's settings), a missing or incomplete directory is a
"not downloaded" error and nothing is ever downloaded, the presence check runs
*before* the ROCm mmap->GPU staging patch is installed, only the predictor
construction runs inside it, the patch is released again when the construction
fails, recognition itself never runs under it, and unloading drops the right
predictors.

Notes:
- Fake `surya.*` and `torch` modules are injected into `sys.modules`, so the
  tests need neither the Surya package, nor Torch, nor a GPU. The fake
  `surya.common.s3.download_directory` raises, so any download attempt fails.
- Whether the staging patch is a no-op on the current host is
  `runtime/rocm_mmap_transfer.py`'s own contract (covered by
  `runtime/test_rocm_mmap_transfer.py`).
  What is pinned down here is only how the service drives it.
"""

from __future__ import annotations

import io
import os
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest.mock import patch

_MODULE_DIR = Path(__file__).resolve().parent
# `modules/ai_backend/ocr/` -> parents[0] = ai_backend, parents[1] = modules,
# parents[2] = program root, the directory `modules.ai_backend...` resolves from.
_PROJECT_ROOT = _MODULE_DIR.parents[2]
if str(_PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(_PROJECT_ROOT))

from modules.ai_backend.ocr import surya as service_module  # noqa: E402
from modules.ai_backend.runtime.model_manager import LoadedModelManager  # noqa: E402

# Surya's own defaults; a load must never reach them.
S3_FOUNDATION_DEFAULT = "s3://text_recognition/2025_09_23"
S3_DETECTOR_DEFAULT = "s3://text_detection/2025_05_07"


class _PatchRecorder:
    """Recording stand-in for `runtime.rocm_mmap_transfer.patched_module_to`.

    Instances are both the factory and the context manager, so `depth` can be
    sampled from inside a constructed predictor to prove the construction
    happened while the patch was installed.
    """

    def __init__(self) -> None:
        self.depth = 0
        self.enters = 0
        self.exits = 0

    def __call__(self) -> "_PatchRecorder":
        return self

    def __enter__(self) -> "_PatchRecorder":
        self.depth += 1
        self.enters += 1
        return self

    def __exit__(self, *exc_info: object) -> bool:
        self.depth -= 1
        self.exits += 1
        return False


class SuryaPredictorLoadTests(unittest.TestCase):
    """Cover the loading seam of `SuryaOcrService`, not its OCR output."""

    def setUp(self) -> None:
        self.recorder = _PatchRecorder()
        # `events` collects (what, argument, patch_depth_when_it_happened).
        self.events: list[tuple[str, object, int]] = []
        self.fail_construction = False

        recorder = self.recorder
        events = self.events
        test_case = self

        tmp = tempfile.TemporaryDirectory(prefix="ms_surya_ocr_")
        self.addCleanup(tmp.cleanup)
        self.rec_dir = str(Path(tmp.name) / "text_recognition")
        self.det_dir = str(Path(tmp.name) / "text_detection")
        Path(self.rec_dir).mkdir()
        Path(self.det_dir).mkdir()
        # Directories whose manifest is complete; the fake `check_manifest` reads it.
        self.ready = {self.rec_dir, self.det_dir}
        settings = types.SimpleNamespace(
            FOUNDATION_MODEL_CHECKPOINT=S3_FOUNDATION_DEFAULT,
            RECOGNITION_MODEL_CHECKPOINT=S3_FOUNDATION_DEFAULT,
            DETECTOR_MODEL_CHECKPOINT=S3_DETECTOR_DEFAULT,
        )
        self.settings = settings

        class FoundationPredictor:
            def __init__(self, checkpoint: object = None, device: object = None, **_kwargs: object) -> None:
                # The settings must already point at the local dir when the
                # library code runs.
                events.append(("foundation", (checkpoint, device, settings.FOUNDATION_MODEL_CHECKPOINT), recorder.depth))
                if test_case.fail_construction:
                    raise RuntimeError("simulated foundation load failure")

        class RecognitionPredictor:
            def __init__(self, foundation: object) -> None:
                events.append(("recognition", None, recorder.depth))

        class DetectionPredictor:
            def __init__(self, checkpoint: object = None, device: object = None, **_kwargs: object) -> None:
                events.append(("detection", (checkpoint, device, settings.DETECTOR_MODEL_CHECKPOINT), recorder.depth))
                if test_case.fail_construction:
                    raise RuntimeError("simulated detection load failure")

        def check_manifest(local_dir: str) -> bool:
            events.append(("check", local_dir, recorder.depth))
            return local_dir in test_case.ready

        def download_directory(remote_dir: str, local_dir: str) -> None:
            raise AssertionError(f"download_directory must never be called ({remote_dir} -> {local_dir})")

        surya = types.ModuleType("surya")
        foundation_mod = types.ModuleType("surya.foundation")
        foundation_mod.FoundationPredictor = FoundationPredictor
        recognition_mod = types.ModuleType("surya.recognition")
        recognition_mod.RecognitionPredictor = RecognitionPredictor
        detection_mod = types.ModuleType("surya.detection")
        detection_mod.DetectionPredictor = DetectionPredictor
        settings_mod = types.ModuleType("surya.settings")
        settings_mod.settings = settings
        common_mod = types.ModuleType("surya.common")
        s3_mod = types.ModuleType("surya.common.s3")
        s3_mod.check_manifest = check_manifest
        s3_mod.download_directory = download_directory
        surya.foundation = foundation_mod
        surya.recognition = recognition_mod
        surya.detection = detection_mod
        surya.settings = settings_mod
        surya.common = common_mod
        common_mod.s3 = s3_mod

        # A bare fake `torch` keeps `_clear_torch_cache` from initializing a real
        # CUDA/HIP context: it has neither `cuda` nor `mps`, so both guards fail.
        fake_torch = types.ModuleType("torch")

        modules_patch = patch.dict(
            sys.modules,
            {
                "surya": surya,
                "surya.foundation": foundation_mod,
                "surya.recognition": recognition_mod,
                "surya.detection": detection_mod,
                "surya.settings": settings_mod,
                "surya.common": common_mod,
                "surya.common.s3": s3_mod,
                "torch": fake_torch,
            },
        )
        modules_patch.start()
        self.addCleanup(modules_patch.stop)

        staging_patch = patch.object(service_module, "patched_module_to", self.recorder)
        staging_patch.start()
        self.addCleanup(staging_patch.stop)

        self.service = service_module.SuryaOcrService(LoadedModelManager())

    def _load_recognition(self, device: str = "cuda:0"):
        return self.service._ensure_recognition_loaded_locked(device, self.rec_dir)

    def _load_detection(self, device: str = "cuda:0"):
        return self.service._ensure_detection_loaded_locked(device, self.det_dir)

    def test_recognition_loads_the_local_dir_outside_the_patch(self) -> None:
        predictor = self._load_recognition()

        self.assertIsNotNone(predictor)
        # Depth 0 for the presence check, depth 1 for both constructions: the
        # process-global patch must cover the weight transfer and nothing else.
        self.assertEqual(
            self.events,
            [
                ("check", self.rec_dir, 0),
                ("foundation", (self.rec_dir, "cuda:0", self.rec_dir), 1),
                ("recognition", None, 1),
            ],
        )
        self.assertEqual(self.settings.RECOGNITION_MODEL_CHECKPOINT, self.rec_dir)
        self.assertEqual(self.settings.DETECTOR_MODEL_CHECKPOINT, S3_DETECTOR_DEFAULT)
        self.assertEqual(self.recorder.enters, 1)
        self.assertEqual(self.recorder.depth, 0, "staging patch must be released")

    def test_detection_loads_the_local_dir_outside_the_patch(self) -> None:
        self._load_detection()

        self.assertEqual(
            self.events,
            [("check", self.det_dir, 0), ("detection", (self.det_dir, "cuda:0", self.det_dir), 1)],
        )
        self.assertEqual(self.recorder.enters, 1)
        self.assertEqual(self.recorder.depth, 0, "staging patch must be released")

    def test_incomplete_checkpoint_is_not_downloaded_and_never_installs_the_patch(self) -> None:
        self.ready.discard(self.rec_dir)

        with self.assertRaises(FileNotFoundError) as raised:
            self._load_recognition()

        self.assertIn("not downloaded", str(raised.exception))
        self.assertEqual(self.events, [("check", self.rec_dir, 0)])
        self.assertEqual(self.recorder.enters, 0)
        self.assertEqual(self.service.health()["last_error"], str(raised.exception))
        self.assertEqual(self.settings.FOUNDATION_MODEL_CHECKPOINT, S3_FOUNDATION_DEFAULT)

    def test_missing_directories_are_explicit_errors(self) -> None:
        missing = str(Path(self.rec_dir) / "absent")
        with self.assertRaises(FileNotFoundError):
            self.service._ensure_recognition_loaded_locked("cuda:0", missing)
        with self.assertRaises(FileNotFoundError):
            self.service._ensure_detection_loaded_locked("cuda:0", missing)
        with self.assertRaises(ValueError):
            self.service._ensure_recognition_loaded_locked("cuda:0", "")
        self.assertEqual(self.recorder.enters, 0)
        self.assertFalse(Path(missing).exists())

    def test_cached_predictors_do_not_reload_or_reinstall_the_patch(self) -> None:
        self._load_recognition()
        self._load_recognition()

        self.assertEqual(len(self.events), 3, "second call must reuse the predictor")
        self.assertEqual(self.recorder.enters, 1)

    def test_device_change_reloads(self) -> None:
        self._load_recognition("cuda:0")
        self._load_recognition("cpu")

        self.assertEqual([event[0] for event in self.events][3:], ["check", "foundation", "recognition"])
        self.assertEqual(self.recorder.enters, 2)
        self.assertEqual(self.recorder.depth, 0)

    def test_directory_change_reloads(self) -> None:
        self._load_recognition()
        other = str(Path(self.rec_dir).parent / "other_recognition")
        Path(other).mkdir()
        self.ready.add(other)

        self.service._ensure_recognition_loaded_locked("cuda:0", other)

        self.assertEqual(self.events[3:5], [("check", other, 0), ("foundation", (other, "cuda:0", other), 1)])
        self.assertEqual(self.service._recognition_dir, other)

    def test_staging_patch_released_when_load_fails(self) -> None:
        self.fail_construction = True

        with self.assertRaises(RuntimeError):
            self._load_recognition()
        with self.assertRaises(RuntimeError):
            self._load_detection()

        self.assertEqual(self.recorder.enters, 2)
        self.assertEqual(self.recorder.exits, 2)
        self.assertEqual(self.recorder.depth, 0)

    def test_recognition_runs_outside_the_staging_patch(self) -> None:
        """Inference must never see the patch: it is process-global and would
        make every activation move pay the `/proc/self/maps` lookup."""
        observed: list[int] = []

        class _FakeImage:
            size = (64, 32)

        def fake_predictor(_images: object, **_kwargs: object) -> list[object]:
            observed.append(self.recorder.depth)
            return [object()]

        self.service._recognize_with_predictors(
            image=_FakeImage(),
            recognition_predictor=fake_predictor,
            detection_predictor=None,
            task_name=service_module.SURYA_TASK_OCR_WITHOUT_BOXES,
            recognize_math=False,
            sort_lines=False,
            drop_repeated_text=False,
            max_sliding_window=None,
            max_tokens=None,
        )

        self.assertEqual(observed, [0])

    def test_unload_drops_every_predictor(self) -> None:
        self._load_recognition()
        self._load_detection()
        model_key = service_module.SuryaOcrService._foundation_model_key("cuda:0")

        self.assertTrue(self.service._unload_foundation_key(model_key))

        self.assertIsNone(self.service._foundation_predictor)
        self.assertIsNone(self.service._recognition_predictor)
        self.assertIsNone(self.service._detection_predictor)
        self.assertIsNone(self.service._device)
        # A second attempt has nothing to evict and must say so.
        self.assertFalse(self.service._unload_foundation_key(model_key))

    def test_unload_of_a_foreign_key_is_refused(self) -> None:
        self._load_recognition()
        other_key = service_module.SuryaOcrService._foundation_model_key("cpu")

        self.assertFalse(self.service._unload_foundation_key(other_key))
        self.assertIsNotNone(self.service._recognition_predictor)

    def test_detector_unload_keeps_the_recognition_predictors(self) -> None:
        self._load_recognition()
        self._load_detection()
        model_key = service_module.SuryaOcrService._detector_model_key("cuda:0")

        self.assertTrue(self.service._unload_detector_key(model_key))

        self.assertIsNone(self.service._detection_predictor)
        self.assertIsNotNone(self.service._recognition_predictor)
        self.assertFalse(self.service._unload_detector_key(model_key))



@unittest.skipUnless(
    os.environ.get("MS_SURYA_RECOGNITION_DIR") and os.environ.get("MS_SURYA_DETECTION_DIR"),
    "set MS_SURYA_RECOGNITION_DIR and MS_SURYA_DETECTION_DIR to run",
)
class RealCheckpointTests(unittest.TestCase):
    """Opt-in: the real Surya predictors (both tasks) load offline from the given directories."""

    def test_recognition_with_boxes_runs_offline(self) -> None:
        import surya.common.s3 as s3
        from PIL import Image

        def no_download(*args: object, **kwargs: object) -> None:
            raise AssertionError(f"download_directory must never be called: {args}")

        image = Image.new("RGB", (64, 32), "white")
        buffer = io.BytesIO()
        image.save(buffer, format="PNG")
        with patch.object(s3, "download_directory", no_download), patch.object(
            service_module, "_resolve_selected_backend_device", lambda _fallback: "cpu"
        ):
            service = service_module.SuryaOcrService(LoadedModelManager())
            result = service.recognize_image_bytes(
                buffer.getvalue(),
                recognition_dir=str(Path(os.environ["MS_SURYA_RECOGNITION_DIR"]).resolve()),
                detection_dir=str(Path(os.environ["MS_SURYA_DETECTION_DIR"]).resolve()),
                task_name=service_module.SURYA_TASK_OCR_WITH_BOXES,
            )
        self.assertIsInstance(result["lines"], list)
        self.assertTrue(service.health()["detector_ready"])


if __name__ == "__main__":
    unittest.main()
