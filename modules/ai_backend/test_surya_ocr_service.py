"""
File: modules/ai_backend/test_surya_ocr_service.py

Purpose:
Unit tests for the Surya OCR predictor loading contract: the checkpoint is
downloaded *before* the ROCm mmap->GPU staging patch is installed, only the
predictor construction runs inside it, the patch is released again when the
construction fails, recognition itself never runs under it, and unloading drops
the right predictors.

Notes:
- Fake `surya.*` and `torch` modules are injected into `sys.modules`, so the
  tests need neither the Surya package, nor Torch, nor a GPU.
- Whether the staging patch is a no-op on the current host is
  `rocm_mmap_transfer`'s own contract (covered by `test_rocm_mmap_transfer.py`).
  What is pinned down here is only how the service drives it.
- The download/patch ordering is the fragile part of the wiring: the patch is
  process-global and `surya`'s `s3://` resolution retries three times with a 5 s
  sleep, so the tests record the patch depth observed at download time as well
  as at construction time.
"""

from __future__ import annotations

import sys
import types
import unittest
from pathlib import Path
from unittest.mock import patch

_MODULE_DIR = Path(__file__).resolve().parent
_PROJECT_ROOT = _MODULE_DIR.parents[1]
if str(_PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(_PROJECT_ROOT))

from modules.ai_backend import surya_ocr_service as service_module  # noqa: E402
from modules.ai_backend.model_manager import LoadedModelManager  # noqa: E402

FOUNDATION_CHECKPOINT = "s3://text_recognition/test"
DETECTOR_CHECKPOINT = "s3://text_detection/test"


class _PatchRecorder:
    """Recording stand-in for `rocm_mmap_transfer.patched_module_to`.

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
        self.fail_download = False

        recorder = self.recorder
        events = self.events
        test_case = self

        class FoundationPredictor:
            def __init__(self, device: object = None, **_kwargs: object) -> None:
                events.append(("foundation", device, recorder.depth))
                if test_case.fail_construction:
                    raise RuntimeError("simulated foundation load failure")

        class RecognitionPredictor:
            def __init__(self, foundation: object) -> None:
                events.append(("recognition", None, recorder.depth))

        class DetectionPredictor:
            def __init__(self, device: object = None, **_kwargs: object) -> None:
                events.append(("detection", device, recorder.depth))
                if test_case.fail_construction:
                    raise RuntimeError("simulated detection load failure")

        surya = types.ModuleType("surya")
        foundation_mod = types.ModuleType("surya.foundation")
        foundation_mod.FoundationPredictor = FoundationPredictor
        recognition_mod = types.ModuleType("surya.recognition")
        recognition_mod.RecognitionPredictor = RecognitionPredictor
        detection_mod = types.ModuleType("surya.detection")
        detection_mod.DetectionPredictor = DetectionPredictor
        settings_mod = types.ModuleType("surya.settings")
        settings_mod.settings = types.SimpleNamespace(
            FOUNDATION_MODEL_CHECKPOINT=FOUNDATION_CHECKPOINT,
            DETECTOR_MODEL_CHECKPOINT=DETECTOR_CHECKPOINT,
        )
        surya.foundation = foundation_mod
        surya.recognition = recognition_mod
        surya.detection = detection_mod
        surya.settings = settings_mod

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
                "torch": fake_torch,
            },
        )
        modules_patch.start()
        self.addCleanup(modules_patch.stop)

        staging_patch = patch.object(service_module, "patched_module_to", self.recorder)
        staging_patch.start()
        self.addCleanup(staging_patch.stop)

        def fake_download(checkpoint: str, *, label: str) -> None:
            events.append(("download", checkpoint, recorder.depth))
            if test_case.fail_download:
                raise RuntimeError(f"simulated {label} download failure")

        download_patch = patch.object(
            service_module, "ensure_checkpoint_downloaded", fake_download
        )
        download_patch.start()
        self.addCleanup(download_patch.stop)

        self.service = service_module.SuryaOcrService(LoadedModelManager())

    def test_recognition_checkpoint_is_downloaded_outside_the_patch(self) -> None:
        predictor = self.service._ensure_recognition_loaded_locked("cuda:0")

        self.assertIsNotNone(predictor)
        # Depth 0 for the download, depth 1 for both constructions: the
        # process-global patch must cover the weight transfer and nothing else.
        self.assertEqual(
            self.events,
            [
                ("download", FOUNDATION_CHECKPOINT, 0),
                ("foundation", "cuda:0", 1),
                ("recognition", None, 1),
            ],
        )
        self.assertEqual(self.recorder.enters, 1)
        self.assertEqual(self.recorder.depth, 0, "staging patch must be released")

    def test_detection_checkpoint_is_downloaded_outside_the_patch(self) -> None:
        self.service._ensure_detection_loaded_locked("cuda:0")

        self.assertEqual(
            self.events,
            [("download", DETECTOR_CHECKPOINT, 0), ("detection", "cuda:0", 1)],
        )
        self.assertEqual(self.recorder.enters, 1)
        self.assertEqual(self.recorder.depth, 0, "staging patch must be released")

    def test_failed_download_never_installs_the_patch(self) -> None:
        self.fail_download = True

        with self.assertRaises(RuntimeError):
            self.service._ensure_recognition_loaded_locked("cuda:0")

        self.assertEqual(self.events, [("download", FOUNDATION_CHECKPOINT, 0)])
        self.assertEqual(self.recorder.enters, 0)

    def test_cached_predictors_do_not_reload_or_reinstall_the_patch(self) -> None:
        self.service._ensure_recognition_loaded_locked("cuda:0")
        self.service._ensure_recognition_loaded_locked("cuda:0")

        self.assertEqual(len(self.events), 3, "second call must reuse the predictor")
        self.assertEqual(self.recorder.enters, 1)

    def test_device_change_reloads_and_redownloads(self) -> None:
        self.service._ensure_recognition_loaded_locked("cuda:0")
        self.service._ensure_recognition_loaded_locked("cpu")

        self.assertEqual([event[0] for event in self.events][3:], ["download", "foundation", "recognition"])
        self.assertEqual(self.recorder.enters, 2)
        self.assertEqual(self.recorder.depth, 0)

    def test_staging_patch_released_when_load_fails(self) -> None:
        self.fail_construction = True

        with self.assertRaises(RuntimeError):
            self.service._ensure_recognition_loaded_locked("cuda:0")
        with self.assertRaises(RuntimeError):
            self.service._ensure_detection_loaded_locked("cuda:0")

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
        self.service._ensure_recognition_loaded_locked("cuda:0")
        self.service._ensure_detection_loaded_locked("cuda:0")
        model_key = service_module.SuryaOcrService._foundation_model_key("cuda:0")

        self.assertTrue(self.service._unload_foundation_key(model_key))

        self.assertIsNone(self.service._foundation_predictor)
        self.assertIsNone(self.service._recognition_predictor)
        self.assertIsNone(self.service._detection_predictor)
        self.assertIsNone(self.service._device)
        # A second attempt has nothing to evict and must say so.
        self.assertFalse(self.service._unload_foundation_key(model_key))

    def test_unload_of_a_foreign_key_is_refused(self) -> None:
        self.service._ensure_recognition_loaded_locked("cuda:0")
        other_key = service_module.SuryaOcrService._foundation_model_key("cpu")

        self.assertFalse(self.service._unload_foundation_key(other_key))
        self.assertIsNotNone(self.service._recognition_predictor)

    def test_detector_unload_keeps_the_recognition_predictors(self) -> None:
        self.service._ensure_recognition_loaded_locked("cuda:0")
        self.service._ensure_detection_loaded_locked("cuda:0")
        model_key = service_module.SuryaOcrService._detector_model_key("cuda:0")

        self.assertTrue(self.service._unload_detector_key(model_key))

        self.assertIsNone(self.service._detection_predictor)
        self.assertIsNotNone(self.service._recognition_predictor)
        self.assertFalse(self.service._unload_detector_key(model_key))


if __name__ == "__main__":
    unittest.main()
