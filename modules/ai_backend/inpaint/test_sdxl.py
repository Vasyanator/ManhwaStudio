"""
File: modules/ai_backend/inpaint/test_sdxl.py

Purpose:
Unit tests for SDXL inpaint parameter normalization and sampler mapping.

Main responsibilities:
- verify mode/model_path/sampler validation raises clear errors;
- verify numeric clamping of steps/cfg/denoise/mask parameters;
- verify the four-channel denoise cap keeps the LaMa prefill meaningful;
- verify sampler names map to the expected diffusers scheduler config;
- verify the model source resolution: a single file needs the config dir, a
  folder needs `model_index.json`, the curated repo id resolves to Rust's
  download, any other repo id is refused, a missing path is explicit;
- verify every load passes `local_files_only=True` (plus `config=` / the fp16
  `variant`) and that no Hugging Face download function is ever called;
- verify the pipeline's host->device move happens inside the ROCm mmap->GPU
  staging patch, and that unloading drops the pipeline and reports it to the
  shared model manager.

These tests cover the pure-Python contract only; they do not load torch,
diffusers, or any model weights. The staging-patch test injects fake `torch` /
`diffusers` modules into `sys.modules` instead, so it needs neither package nor
a GPU. Whether the patch is a no-op on the current host is `rocm_mmap_transfer`'s
own contract (covered by `test_rocm_mmap_transfer.py`); what is pinned here is
only that the service wraps the move in it.
"""

from __future__ import annotations

import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest.mock import patch

from modules.ai_backend.inpaint import sdxl as svc
from modules.ai_backend.runtime.model_manager import SmallModelManager


class NormalizeSdxlParamsTests(unittest.TestCase):
    def _base(self, **overrides: object) -> dict[str, object]:
        params: dict[str, object] = {
            "mode": "nine_channel",
            "model_path": "/models/sdxl-inpaint.safetensors",
            "sampler": "Euler",
        }
        params.update(overrides)
        return params

    def test_valid_params_pass_through(self) -> None:
        out = svc.normalize_sdxl_params(self._base(steps=40, cfg_scale=6.5))
        self.assertEqual(out["mode"], "nine_channel")
        self.assertEqual(out["model_path"], "/models/sdxl-inpaint.safetensors")
        self.assertEqual(out["steps"], 40)
        self.assertAlmostEqual(out["cfg_scale"], 6.5)

    def test_invalid_mode_raises(self) -> None:
        with self.assertRaises(ValueError):
            svc.normalize_sdxl_params(self._base(mode="bogus"))

    def test_empty_model_path_raises(self) -> None:
        with self.assertRaises(ValueError):
            svc.normalize_sdxl_params(self._base(model_path="   "))

    def test_invalid_sampler_raises(self) -> None:
        with self.assertRaises(ValueError):
            svc.normalize_sdxl_params(self._base(sampler="NopeSampler"))

    def test_numeric_clamping(self) -> None:
        out = svc.normalize_sdxl_params(
            self._base(
                steps=9999,
                cfg_scale=999.0,
                denoise_strength=5.0,
                mask_blur=-10,
                mask_dilation=999,
            )
        )
        self.assertEqual(out["steps"], 150)
        self.assertEqual(out["cfg_scale"], 30.0)
        self.assertEqual(out["denoise_strength"], 1.0)
        self.assertEqual(out["mask_blur"], 0)
        self.assertEqual(out["mask_dilation"], 64)

    def test_four_channel_denoise_capped_below_one(self) -> None:
        # Strength 1.0 on a 4-channel model would re-noise the hole to pure noise
        # and discard the LaMa prefill, so it must be capped below 1.0.
        out = svc.normalize_sdxl_params(
            self._base(mode="four_channel", denoise_strength=1.0)
        )
        self.assertLess(out["denoise_strength"], 1.0)

    def test_nine_channel_keeps_full_denoise(self) -> None:
        out = svc.normalize_sdxl_params(self._base(denoise_strength=1.0))
        self.assertEqual(out["denoise_strength"], 1.0)

    def test_seed_default_is_random_sentinel(self) -> None:
        out = svc.normalize_sdxl_params(self._base())
        self.assertEqual(out["seed"], -1)


class ResolveSchedulerConfigTests(unittest.TestCase):
    def test_known_sampler_returns_class_and_kwargs(self) -> None:
        class_name, kwargs = svc.resolve_scheduler_config("DPM++ 2M Karras")
        self.assertEqual(class_name, "DPMSolverMultistepScheduler")
        self.assertTrue(kwargs.get("use_karras_sigmas"))

    def test_returned_kwargs_are_isolated_copies(self) -> None:
        _, kwargs = svc.resolve_scheduler_config("DPM++ 2M")
        kwargs["mutated"] = True
        _, kwargs_again = svc.resolve_scheduler_config("DPM++ 2M")
        self.assertNotIn("mutated", kwargs_again)

    def test_unknown_sampler_raises(self) -> None:
        with self.assertRaises(ValueError):
            svc.resolve_scheduler_config("Unknown")

    def test_all_rust_samplers_are_supported(self) -> None:
        # Mirror of SDXL_SAMPLERS in
        # crates/ms-tab-cleaning/src/tools/ai_editor/engines/sdxl/mod.rs.
        rust_samplers = [
            "Euler",
            "Euler a",
            "DPM++ 2M",
            "DPM++ 2M Karras",
            "DPM++ SDE Karras",
            "DDIM",
            "UniPC",
            "Heun",
        ]
        for sampler in rust_samplers:
            with self.subTest(sampler=sampler):
                class_name, _ = svc.resolve_scheduler_config(sampler)
                self.assertTrue(class_name.endswith("Scheduler"))


try:
    import numpy as _np_for_tests
except Exception:
    _np_for_tests = None


@unittest.skipIf(_np_for_tests is None, "numpy is required")
class MatchVaeRoundtripTests(unittest.TestCase):
    def test_offset_compensates_uniform_darkening(self) -> None:
        np = _np_for_tests
        original = np.full((16, 16, 3), 200, dtype=np.uint8)
        # Generated is uniformly 20 darker everywhere (the VAE roundtrip shift).
        generated = np.full((16, 16, 3), 180.0, dtype=np.float32)
        # Mask the central 4x4 block; the rest is unmasked context.
        alpha = np.zeros((16, 16), dtype=np.float32)
        alpha[6:10, 6:10] = 1.0
        corrected = svc._match_vae_roundtrip(generated, original, alpha)
        # The masked patch should be lifted back toward the original brightness.
        self.assertAlmostEqual(float(corrected[7, 7, 0]), 200.0, delta=1.0)

    def test_skips_when_too_few_unmasked_pixels(self) -> None:
        np = _np_for_tests
        original = np.full((8, 8, 3), 200, dtype=np.uint8)
        generated = np.full((8, 8, 3), 180.0, dtype=np.float32)
        alpha = np.ones((8, 8), dtype=np.float32)  # everything masked
        corrected = svc._match_vae_roundtrip(generated, original, alpha)
        # No reliable context -> generated returned unchanged.
        self.assertTrue(np.allclose(corrected, generated))


class _PatchRecorder:
    """Recording stand-in for `rocm_mmap_transfer.patched_module_to`.

    The instance is both the factory and the context manager, so `depth` can be
    sampled from inside the faked `pipe.to()` to prove the move happened while
    the staging patch was installed.
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


class _FakePipe:
    """Minimal stand-in for `StableDiffusionXLInpaintPipeline`."""

    def __init__(self, recorder: _PatchRecorder) -> None:
        self._recorder = recorder
        # (device, staging patch depth at the time of the move).
        self.moves: list[tuple[object, int]] = []
        self.vae = types.SimpleNamespace(config=types.SimpleNamespace(force_upcast=False))

    def to(self, device: object) -> "_FakePipe":
        self.moves.append((device, self._recorder.depth))
        return self

    def set_progress_bar_config(self, **_kwargs: object) -> None:
        pass

    def enable_attention_slicing(self) -> None:
        pass


class SdxlPipelinePlacementTests(unittest.TestCase):
    """Pin the ROCm mmap staging seam of the SDXL pipeline build."""

    def setUp(self) -> None:
        self.recorder = _PatchRecorder()
        recorder = self.recorder
        pipes: list[_FakePipe] = []
        self.pipes = pipes

        # (loader name, path, kwargs) of every pipeline load.
        loads: list[tuple[str, str, dict[str, object]]] = []
        self.loads = loads

        def _load(name: str, path: str, kwargs: dict[str, object]) -> _FakePipe:
            loads.append((name, path, dict(kwargs)))
            pipe = _FakePipe(recorder)
            pipes.append(pipe)
            return pipe

        class StableDiffusionXLInpaintPipeline:
            @staticmethod
            def from_pretrained(path: str, **kwargs: object) -> _FakePipe:
                return _load("from_pretrained", path, kwargs)

            @staticmethod
            def from_single_file(path: str, **kwargs: object) -> _FakePipe:
                return _load("from_single_file", path, kwargs)

        diffusers = types.ModuleType("diffusers")
        diffusers.StableDiffusionXLInpaintPipeline = StableDiffusionXLInpaintPipeline

        def _no_download(*args: object, **kwargs: object) -> None:
            raise AssertionError(f"snapshot_download must never be called: {args} {kwargs}")

        hub = types.ModuleType("huggingface_hub")
        hub.snapshot_download = _no_download
        hub.hf_hub_download = _no_download

        # `float16` only needs identity, and the absence of `cuda` keeps
        # `_clear_torch_cache` from initializing a real accelerator context.
        fake_torch = types.ModuleType("torch")
        fake_torch.float16 = object()
        fake_torch.float32 = object()

        modules_patch = patch.dict(
            sys.modules, {"diffusers": diffusers, "torch": fake_torch, "huggingface_hub": hub}
        )
        modules_patch.start()
        self.addCleanup(modules_patch.stop)

        staging_patch = patch.object(svc, "patched_module_to", self.recorder)
        staging_patch.start()
        self.addCleanup(staging_patch.stop)

        self.fake_torch = fake_torch

    def test_move_to_gpu_runs_inside_staging_patch(self) -> None:
        # A pre-resolved source keeps the build itself off the filesystem.
        pipe = svc._build_sdxl_inpaint_pipeline(svc.SdxlModelSource(kind="folder", path="/m/sdxl"), "cuda:0")

        self.assertIs(pipe, self.pipes[0])
        self.assertEqual(pipe.moves, [("cuda:0", 1)])
        self.assertEqual((self.recorder.enters, self.recorder.exits), (1, 1))
        self.assertEqual(self.recorder.depth, 0)

    def test_fp16_pipeline_keeps_vae_force_upcast(self) -> None:
        # The staging wrapper must not disturb the fp16 VAE contract.
        pipe = svc._build_sdxl_inpaint_pipeline(svc.SdxlModelSource(kind="folder", path="/m/sdxl"), "cuda:0")
        self.assertTrue(pipe.vae.config.force_upcast)

    def test_every_source_loads_offline(self) -> None:
        sources = [
            svc.SdxlModelSource(kind="single_file", path="/m/a.safetensors", config_dir="/m/cfg"),
            svc.SdxlModelSource(kind="folder", path="/m/folder"),
            svc.SdxlModelSource(kind="curated", path="/m/curated", variant=svc.CURATED_INPAINT_VARIANT),
        ]
        for source in sources:
            svc._build_sdxl_inpaint_pipeline(source, "cpu")

        dtype = self.fake_torch.float32
        self.assertEqual(
            self.loads,
            [
                ("from_single_file", "/m/a.safetensors", {"config": "/m/cfg", "local_files_only": True, "torch_dtype": dtype}),
                ("from_pretrained", "/m/folder", {"local_files_only": True, "torch_dtype": dtype}),
                ("from_pretrained", "/m/curated", {"variant": "fp16", "local_files_only": True, "torch_dtype": dtype}),
            ],
        )


class SdxlModelSourceTests(unittest.TestCase):
    """Pin `resolve_model_source`: local paths only, plus the one curated repo id."""

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory(prefix="ms_sdxl_src_")
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.config_dir = self._diffusers_dir("config")
        self.checkpoint = self.root / "model.safetensors"
        self.checkpoint.write_bytes(b"")

    def _diffusers_dir(self, name: str) -> Path:
        path = self.root / name
        path.mkdir()
        (path / "model_index.json").write_text("{}", encoding="utf-8")
        return path

    def test_single_file_takes_the_config_dir(self) -> None:
        source = svc.resolve_model_source(str(self.checkpoint), config_dir=str(self.config_dir), curated_model_dir=None)

        self.assertEqual(source, svc.SdxlModelSource(kind="single_file", path=str(self.checkpoint), config_dir=str(self.config_dir)))

    def test_single_file_without_a_downloaded_config_is_refused(self) -> None:
        missing = self.root / "absent"
        with self.assertRaises(FileNotFoundError) as raised:
            svc.resolve_model_source(str(self.checkpoint), config_dir=str(missing), curated_model_dir=None)
        self.assertIn("не скачана", str(raised.exception))
        self.assertFalse(missing.exists())
        with self.assertRaises(ValueError):
            svc.resolve_model_source(str(self.checkpoint), config_dir="", curated_model_dir=None)
        with self.assertRaises(ValueError):
            svc.resolve_model_source(str(self.checkpoint), config_dir="relative/cfg", curated_model_dir=None)

    def test_wrong_file_suffix_is_refused(self) -> None:
        other = self.root / "model.bin"
        other.write_bytes(b"")
        with self.assertRaises(FileNotFoundError):
            svc.resolve_model_source(str(other), config_dir=str(self.config_dir), curated_model_dir=None)

    def test_folder_needs_model_index(self) -> None:
        folder = self._diffusers_dir("folder")
        self.assertEqual(
            svc.resolve_model_source(str(folder), config_dir=str(self.config_dir), curated_model_dir=None),
            svc.SdxlModelSource(kind="folder", path=str(folder)),
        )
        bare = self.root / "bare"
        bare.mkdir()
        with self.assertRaises(FileNotFoundError):
            svc.resolve_model_source(str(bare), config_dir=str(self.config_dir), curated_model_dir=None)

    def test_curated_repo_id_resolves_to_the_downloaded_dir(self) -> None:
        curated = self._diffusers_dir("curated")
        source = svc.resolve_model_source(
            svc.CURATED_INPAINT_REPO_ID, config_dir=str(self.config_dir), curated_model_dir=str(curated)
        )
        self.assertEqual(source, svc.SdxlModelSource(kind="curated", path=str(curated), variant="fp16"))

    def test_curated_repo_id_without_its_download_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            svc.resolve_model_source(svc.CURATED_INPAINT_REPO_ID, config_dir=str(self.config_dir), curated_model_dir=None)
        with self.assertRaises(FileNotFoundError):
            svc.resolve_model_source(
                svc.CURATED_INPAINT_REPO_ID, config_dir=str(self.config_dir), curated_model_dir=str(self.root / "absent")
            )

    def test_other_repo_ids_are_refused(self) -> None:
        for repo_id in ("stabilityai/stable-diffusion-xl-base-1.0", "org/sdxl-inpaint", "model"):
            with self.subTest(repo_id=repo_id), self.assertRaises(ValueError):
                svc.resolve_model_source(repo_id, config_dir=str(self.config_dir), curated_model_dir=None)

    def test_missing_absolute_path_is_explicit(self) -> None:
        with self.assertRaises(FileNotFoundError):
            svc.resolve_model_source(str(self.root / "gone.safetensors"), config_dir=str(self.config_dir), curated_model_dir=None)


class SdxlUnloadTests(unittest.TestCase):
    """Pin the unload bookkeeping; no torch, diffusers or weights involved."""

    def test_unload_drops_the_pipeline_and_reports_it(self) -> None:
        service = svc.SdxlInpaintService(SmallModelManager(), object())
        service._pipe = object()
        service._active_model_key = "sdxl:nine_channel:cuda:0:/models/a.safetensors"

        with patch.object(service._model_manager, "mark_unloaded") as unloaded:
            self.assertTrue(service.unload())

        unloaded.assert_called_once_with("sdxl:nine_channel:cuda:0:/models/a.safetensors")
        self.assertIsNone(service._pipe)
        self.assertIsNone(service._active_model_key)

    def test_unload_without_a_pipeline_is_a_noop(self) -> None:
        service = svc.SdxlInpaintService(SmallModelManager(), object())
        self.assertFalse(service.unload())

    def test_unload_key_refuses_a_foreign_key(self) -> None:
        service = svc.SdxlInpaintService(SmallModelManager(), object())
        service._pipe = object()
        service._active_model_key = "sdxl:a"

        self.assertFalse(service._unload_key("sdxl:b"))
        self.assertIsNotNone(service._pipe)


if __name__ == "__main__":
    unittest.main()
